//! `kanshou` (観照 — contemplation/introspection) — live process
//! introspection over Unix sockets.
//!
//! Every long-running pleme-io binary opens a kanshou socket at a
//! canonical path on startup. Operators, MCP servers, and other
//! processes query the live `AppState` over that socket: sessions in
//! flight, atomic frame counters, queue depths, "what config did you
//! actually load", whatever the binary registers.
//!
//! Closes the "I have an MCP but no wire into the live process" class:
//! mado MCP returning process-local zeros while the GUI mado renders
//! a hundred frames a second, tear MCP showing zero sessions while
//! embedded tear sees one, "is tend processing X right now?" requiring
//! `pgrep + lsof` archaeology.
//!
//! ## Three modules
//!
//! - [`types`] — wire schema (`Query`, `QueryResult`, `Introspect`
//!   trait). Stable serde shape.
//! - [`server`] — [`Server<T>`](server::Server). Wrap an
//!   `Arc<T: Introspect>`, call `.serve()`, the socket appears at the
//!   canonical path and accepts queries.
//! - [`client`] — [`Client`](client::Client) + [`discover`](client::discover).
//!   Connect to a running binary by name+pid or auto-discover.
//!
//! ## Canonical socket path
//!
//! - macOS: `$HOME/Library/Application Support/kanshou/<app>-<pid>.sock`
//! - linux: `$XDG_RUNTIME_DIR/kanshou/<app>-<pid>.sock`
//!   (falls back to `/tmp/kanshou-<uid>` if XDG_RUNTIME_DIR unset)
//!
//! ## Wire protocol
//!
//! Length-prefixed JSON-RPC. Each frame is `u32 BE length` then JSON
//! bytes. Request: serialized [`types::Query`]. Response: serialized
//! [`types::QueryResult`]. Connection stays open across multiple
//! request/response cycles; the server closes only on EOF or hard I/O
//! error.
//!
//! ## Minimal consumer example
//!
//! ```no_run
//! use std::sync::Arc;
//! use kanshou::{server::Server, types::{Introspect, Query, QueryResult, QueryError}};
//!
//! struct AppState { sessions: Vec<String>, frame_count: u64 }
//!
//! impl Introspect for AppState {
//!     fn query(&self, q: &Query) -> QueryResult {
//!         match q.path.as_slice() {
//!             [first] if first == "sessions" => Ok(serde_json::to_value(&self.sessions).unwrap()),
//!             [first] if first == "frame_count" => Ok(serde_json::to_value(self.frame_count).unwrap()),
//!             _ => Err(QueryError::unknown_field(q.path.join("."))),
//!         }
//!     }
//!     fn schema(&self) -> &'static [&'static str] {
//!         &["sessions", "frame_count"]
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let state = Arc::new(AppState { sessions: vec![], frame_count: 0 });
//!     let server = Server::new("myapp", state)?;
//!     server.serve().await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Theory
//!
//! Phase 1 of the fleet-wide live-introspection wave (next phases:
//! `#[derive(Introspect)]` in gen-macros, mado/tear retrofit, fleet
//! rollout, `gen kanshou` operator CLI). The substrate gains one
//! typed primitive: every running pleme-io process becomes queryable
//! by construction.

#![doc(html_root_url = "https://docs.rs/kanshou/0.1.0")]

pub mod client;
pub mod path;
pub mod server;
pub mod types;

pub use client::{discover, Client, DiscoveredInstance};
pub use server::Server;
pub use types::{Introspect, Query, QueryError, QueryResult};

#[cfg(feature = "derive")]
pub use kanshou_derive::Introspect;
