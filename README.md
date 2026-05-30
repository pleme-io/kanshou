# kanshou (観照)

> Live process introspection over Unix sockets — every pleme-io binary
> becomes queryable by construction.

`kanshou` is the substrate primitive that closes the **"I have an MCP
but no wire into the live process"** class. Wrap your app's `Arc<State>`,
expose it through a socket at a canonical path, and any operator tool,
MCP server, or sibling process queries the live state without
log-archaeology or `pgrep`-fu.

## Why

- Mado MCP reporting `frame_perf: 0` while the GUI mado renders 120 fps
- Tear MCP showing zero sessions while the embedded tear in mado has one
- `tend reconcile` doing _something_ for three minutes with no way to
  see _what_
- `kindling` posture queries returning a snapshot that's already stale
  by the time it lands in your shell

All the same class. Each MCP / tool reads process-local state, but the
state lives in a different process.

## What

```rust
use std::sync::Arc;
use kanshou::{Server, Introspect, Query, QueryResult, QueryError};

struct AppState {
    sessions: Vec<String>,
    frame_count: u64,
}

impl Introspect for AppState {
    fn query(&self, q: &Query) -> QueryResult {
        match q.path.as_slice() {
            [k] if k == "sessions" => Ok(serde_json::to_value(&self.sessions).unwrap()),
            [k] if k == "frame_count" => Ok(serde_json::to_value(self.frame_count).unwrap()),
            _ => Err(QueryError::unknown_field(q.path.join("."))),
        }
    }
    fn schema(&self) -> &'static [&'static str] {
        &["sessions", "frame_count"]
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = Arc::new(AppState { sessions: vec![], frame_count: 0 });
    let server = Server::new("myapp", state)?;
    println!("kanshou listening at {}", server.socket_path().display());
    server.serve().await?;
    Ok(())
}
```

And the operator side:

```rust
use kanshou::{discover, Client, Query};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    for inst in discover(Some("mado")) {
        let mut client = Client::connect(&inst.socket_path).await?;
        let v = client.query(&Query::field(["frame_count"])).await??;
        println!("mado pid={}: frame_count={v}", inst.pid);
    }
    Ok(())
}
```

## Canonical socket path

- macOS: `$HOME/Library/Application Support/kanshou/<app>-<pid>.sock`
- linux: `$XDG_RUNTIME_DIR/kanshou/<app>-<pid>.sock`
  (falls back to `/tmp/kanshou-<uid>` when XDG_RUNTIME_DIR unset)

## Wire protocol

Length-prefixed JSON-RPC. Each frame is `u32 BE length` then JSON
bytes. Request is a serialized [`Query`]; response is a serialized
[`QueryResult`]. Connection stays open across multiple queries —
clients fire as many as they want before disconnecting.

## Roadmap

This crate is **phase 1** of the fleet-wide live-introspection wave:

| Phase | What |
|---|---|
| 1 (this crate) | `kanshou-core` — Server, Client, discovery |
| 2 | `#[derive(Introspect)]` in `gen-macros` — every `pub` field becomes a queryable leaf, every `&self` method becomes a callable |
| 3 | `mado` + `tear` retrofit — first two consumers, validates the wire |
| 4 | Fleet sweep — tend, kindling, kasou, engenho, tatara, vigy, blackmatter-cli, … all expose their AppState |
| 5 | `gen kanshou` operator CLI — "show me every introspectable process on this host" + typed queries |

## License

MIT
