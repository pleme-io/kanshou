# kanshou retrofit cookbook

> The five-step recipe for adding kanshou introspection to any
> existing pleme-io Rust binary. Proven on mado (GUI), frost
> (shell), tear-daemon (multiplexer), tend (reconciler daemon),
> gen-cli (operator tool). Each retrofit is ~30 minutes following
> this recipe.

## When to use

Add kanshou to any long-running binary that needs operator visibility:
daemons, GUIs, REPL-style shells, multiplexers, reconcilers,
coordinators. Skip placeholder/stub binaries and short-lived CLIs.

## The recipe

### Step 1 — Cargo.toml

Add to `[dependencies]`:

```toml
kanshou = { git = "https://github.com/pleme-io/kanshou" }
parking_lot = "0.12"  # only if you use RwLock around Option<String> state
```

If the binary doesn't already pull tokio, add it too:

```toml
tokio = { version = "1", features = ["rt", "macros", "net", "io-util", "time"] }
```

### Step 2 — `src/kanshou_state.rs`

Drop in this template (rename per-binary, extend the leaves):

```rust
//! `<Binary>State` — the aggregator the kanshou server exposes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kanshou::{Introspect, Query, QueryError, QueryResult};

pub struct <Binary>State {
    pub started_at_unix_ms: u64,
    // Add atomics + Arc<RwLock<Option<String>>> for live counters.
}

impl <Binary>State {
    #[must_use]
    pub fn new() -> Self {
        Self {
            started_at_unix_ms: now_unix_ms(),
        }
    }
}

impl Default for <Binary>State {
    fn default() -> Self { Self::new() }
}

impl Introspect for <Binary>State {
    fn query(&self, q: &Query) -> QueryResult {
        let Some(first) = q.path.first().map(String::as_str) else {
            return Err(QueryError::unknown_field(String::new()));
        };
        let now = now_unix_ms();
        match first {
            "process" => Ok(serde_json::json!({
                "pid": std::process::id(),
                "binary": std::env::current_exe()
                    .ok().map(|p| p.display().to_string()).unwrap_or_default(),
                "started_at_unix_ms": self.started_at_unix_ms,
                "uptime_ms": now.saturating_sub(self.started_at_unix_ms),
                "version": env!("CARGO_PKG_VERSION"),
            })),
            // Per-binary leaves go here. Each leaf = one match arm.
            other => Err(QueryError::unknown_field(other.to_string())),
        }
    }

    fn schema(&self) -> &'static [&'static str] {
        &["process"]
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub fn spawn_server(
    app_name: &str,
    state: Arc<<Binary>State>,
) -> std::io::Result<std::path::PathBuf> {
    let server = kanshou::Server::new(app_name, state)?;
    let socket_path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            tracing::warn!(error = ?e, "kanshou server exited with error");
        }
    });
    Ok(socket_path)
}
```

### Step 3 — wire it in `main`

For `#[tokio::main]` binaries, drop this near the top of `main()`:

```rust
mod kanshou_state;

#[tokio::main]
async fn main() -> Result<()> {
    // ... existing setup ...

    let kanshou_state = std::sync::Arc::new(
        kanshou_state::<Binary>State::new(),
    );
    match kanshou_state::spawn_server("<binary-name>", std::sync::Arc::clone(&kanshou_state)) {
        Ok(path) => tracing::info!(socket = %path.display(), "kanshou introspection live"),
        Err(e) => tracing::warn!(err = %e, "kanshou bind failed; introspection disabled"),
    }

    // ... existing main body, threading kanshou_state through where useful ...
}
```

For non-tokio binaries (synchronous `main` like frost), spawn a
dedicated thread that owns a current-thread tokio runtime:

```rust
std::thread::Builder::new()
    .name("<binary>-kanshou".into())
    .spawn(move || {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(async {
                if let Ok(path) = kanshou_state::spawn_server("<binary>", state) {
                    tracing::info!(socket = %path.display(), "kanshou live");
                    std::future::pending::<()>().await;
                }
            }),
            Err(e) => tracing::warn!(err = %e, "kanshou tokio runtime failed"),
        }
    })
    .ok();
```

### Step 4 — tick the counters

Wherever the binary does meaningful work — cycle boundary, queue
drain, render frame — call `state.<counter>.fetch_add(1, Ordering::Relaxed)`
or `state.<field>.write().replace(value)`. Best place is the natural
boundary where you'd `tracing::info!("did X")` today.

For lock-protected current-job state, set on entry and clear on exit:

```rust
state.current_workspace.write().replace(ws.name.clone());
// ... do work ...
state.current_workspace.write().take();
```

### Step 5 — verify

Build, run the binary, then from any shell:

```bash
gen kanshou list
gen kanshou query <binary-name> process
gen kanshou query <binary-name> <your-new-leaf>
```

If the socket appears in `gen kanshou list` and a query against
`process` returns the JSON shape, the retrofit is complete.

## Common shapes

| Leaf | Purpose | Wire shape |
|---|---|---|
| `process` | Always present; pid, binary, started_at, uptime, version | `{ pid, binary, started_at_unix_ms, uptime_ms, version }` |
| `ticks` | Loop counter + freshness | `{ completed, last_tick_unix_ms, ms_since_last }` |
| `sessions` / `panes` / `jobs` | Live work-unit registries | `{ count, items: [...] }` |
| `current` | What's in flight right now | `{ <field>: Option<String>, ... }` |
| `frame_perf` (GUI) | Render hot-path atomics | `{ last_frame_us, total_frames, total_frames_skipped }` |
| `vt` (terminal client) | Pending VT response queries | `{ pending_responses: u64 }` |
| `config` | Live config snapshot | the consumer's MadoConfig / etc. |
| `rc` (shell) | rc-load posture | `{ loaded: bool, path: Option<String> }` |

## Anti-patterns

- **Don't** lock the AppState behind a single Mutex — each leaf
  should be independently queryable. Use Arc<Atomic*> per counter
  and Arc<RwLock<Option<T>>> per nullable scalar so concurrent
  queries don't contend.
- **Don't** authenticate connections inside the consumer. The
  socket lives in a user-private directory; OS filesystem perms
  ARE the auth. Adding bearer tokens is over-engineering.
- **Don't** stream large payloads. Each query is one request/
  response. If a leaf needs to return MB of grid bytes, expose a
  method-call leaf that takes a small parameter (e.g. session id)
  and returns a bounded snapshot.
- **Don't** depend on the kanshou socket for correctness. Bind
  failures should warn-log and continue. If the operator can't
  query, the daemon should still daemon.

## What you get

After the retrofit, any operator tool, MCP server, or sibling
process can connect to your binary's socket and query the live
state without log archaeology. The substrate compounds: every new
field becomes an MCP tool surface free; every new daemon becomes
queryable via `gen kanshou query <name> <field>`.
