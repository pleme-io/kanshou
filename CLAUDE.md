# kanshou (観照)

Live process introspection over Unix sockets — the substrate primitive
that closes the "MCP server has no wire into the live GUI/daemon" class.

## Layout

A two-member workspace: `crates/kanshou` (the library) and
`crates/kanshou-derive` (`#[derive(Introspect)]`, re-exported by the
library under its default `derive` feature). Inside `crates/kanshou`:

- `src/lib.rs` — module index, re-exports
- `src/types.rs` — `Introspect` trait, `Query`, `QueryResult`, `QueryError`
- `src/path.rs` — canonical socket path resolution (darwin + linux)
- `src/server.rs` — `Server<T: Introspect>`, accept loop, per-connection handler
- `src/client.rs` — `discover()`, `Client::connect`, `Client::query`
- `src/mcp.rs` — forward an MCP tool's query to the live consumer
- `src/metrics.rs` — `Counter`, `Gauge`, `LogHistogram`, `Family` and
  `metric_labels!`: the values a consumer exports through `Introspect`
- `tests/roundtrip.rs` — end-to-end server↔client query test

## Wire protocol

Length-prefixed JSON-RPC. Each frame is `u32 BE length` then JSON
bytes. Request: serialized `Query`. Response: serialized `QueryResult`.
4 MiB cap per frame.

## Phases (in the wave)

| Phase | Where |
|---|---|
| 1 — kanshou-core | THIS REPO |
| 2 — `#[derive(Introspect)]` | `pleme-io/gen` (gen-macros) |
| 3 — mado + tear retrofit | `pleme-io/mado`, `pleme-io/tear` |
| 4 — fleet sweep | tend, kindling, kasou, engenho, tatara, vigy, blackmatter-cli, … |
| 5 — operator CLI | `gen kanshou` subcommand |

## Conventions

- Two typed surfaces: the `Introspect` trait (how a process answers) and
  `metrics` (what it answers with). A metric type joins `metrics` only
  when it is lock-free to record and `const`-constructible.
- `#[derive(Serialize, Deserialize)]` on every wire type — wire shape
  is the API.
- Each test creates a per-test-process socket name (`kanshou-test-<pid>`)
  so concurrent test runs never clash on the same path.

## Anti-patterns

- Authenticating connections inside `kanshou`. The Unix socket lives
  in a user-private directory (`$HOME/...` or `$XDG_RUNTIME_DIR`); the
  OS filesystem perms ARE the auth. Adding bearer tokens or capability
  envelopes inside `kanshou` is over-engineering for v1.
- Cross-process pub/sub through this socket. The `Query`/`Response`
  shape is request/response, deliberately. Streaming (live frame-by-frame
  introspection, queue depth deltas) lands in a sibling crate when a
  consumer earns it — not bolted onto v1.
- Reaching back into the `Client` after a query error. The Tokio `UnixStream`
  is half-duplex per-frame; an error means the stream state is undefined.
  Drop and reconnect.
