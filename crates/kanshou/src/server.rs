//! Unix-socket introspection server.
//!
//! Wrap an `Arc<T: Introspect>`, call `Server::serve()`, and the
//! socket appears at the canonical path. Each connection accepts an
//! arbitrary number of length-prefixed JSON queries until the client
//! disconnects.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::path::socket_path;
use crate::types::{Introspect, Query, QueryError, QueryResult};

/// A running kanshou server. Owns the listener and unlinks the socket
/// file on drop.
pub struct Server<T: Introspect + 'static> {
    state: Arc<T>,
    listener: UnixListener,
    socket_path: PathBuf,
}

impl<T: Introspect + 'static> Server<T> {
    /// Create + bind. Idempotent on a stale socket file (unlinks it
    /// first if no process is holding it). Fails when the parent
    /// directory can't be created or when bind fails for a reason
    /// other than `EADDRINUSE-with-no-listener`.
    pub fn new(app_name: &str, state: Arc<T>) -> std::io::Result<Self> {
        let path = socket_path(app_name, std::process::id());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Clean a stale socket from a previous run. If a live
        // process is holding it, `bind` will still fail and we
        // surface the error.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        Ok(Self {
            state,
            listener,
            socket_path: path,
        })
    }

    /// The path the server is listening on. Useful for tests and for
    /// announcing where the socket lives.
    #[must_use]
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Bind, then run the whole introspection sidecar on its own thread —
    /// the one call an application needs.
    ///
    /// # Why this lives here and not in each application
    ///
    /// Three fleet binaries (mado, tear-daemon, frost) each hand-rolled this:
    /// a named `std::thread`, a current-thread tokio runtime built inside it,
    /// `Server::new` + `serve()` under `block_on`, and a `pending()` to hold
    /// the thread open. The three bodies were byte-identical except for one
    /// log literal — and they still managed to disagree about the thing that
    /// matters.
    ///
    /// All three carried a doc comment promising that a failed sidecar is
    /// non-fatal ("bind failure is non-fatal — the app runs without the
    /// socket"). Two of them then ended the spawn with `.expect(...)`. Under
    /// thread-spawn `EAGAIN` — fd or thread exhaustion, which is exactly when
    /// you most want introspection — mado and tear-daemon **panicked during
    /// startup**, turning a missing debug socket into a dead terminal and a
    /// dead multiplexer daemon. frost, alone, degraded as documented.
    ///
    /// This method makes that decision once. **Every failure path here is
    /// non-fatal by construction**: it returns `None` and logs, and there is
    /// no arm that can panic. A caller cannot re-introduce the divergence
    /// because a caller no longer writes the spawn.
    ///
    /// # What the return value means
    ///
    /// `Some(path)` — the socket is bound and being served on a background
    /// thread that outlives this call. `None` — introspection is unavailable
    /// and the reason has been logged. The application continues either way;
    /// that is the whole contract.
    ///
    /// # The bind happens inside the thread, and the result is sent back
    ///
    /// `tokio::net::UnixListener::bind` **panics outside a runtime** ("there
    /// is no reactor running"), so the bind cannot happen on the calling
    /// thread — which is exactly why all three hand-rolled copies did it
    /// inside their own runtime. A first draft of this method bound eagerly
    /// and paniced in every caller; the test below is what caught it.
    ///
    /// So the thread binds, reports the outcome back over a channel, and only
    /// then serves. The calling thread waits for that one message, which is
    /// why `Some(path)` still means *the socket is bound*, not merely *a
    /// thread was started*. Callers announce the path as "introspection live",
    /// and a path that might not exist would make that a lie.
    ///
    /// # Errors
    ///
    /// None — this is infallible on purpose. See above.
    #[must_use]
    pub fn spawn_sidecar(app_name: &str, state: Arc<T>) -> Option<PathBuf> {
        let (tx, rx) = std::sync::mpsc::channel::<Option<PathBuf>>();
        let app = app_name.to_owned();

        let spawned = std::thread::Builder::new()
            .name("kanshou".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .thread_name("kanshou-tokio")
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!(err = %e, "could not create kanshou tokio runtime");
                        let _ = tx.send(None);
                        return;
                    }
                };
                rt.block_on(async move {
                    // Inside the runtime: bind is legal here and nowhere else.
                    let server = match Self::new(&app, state) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(
                                app = %app,
                                err = %e,
                                "kanshou bind failed; introspection disabled",
                            );
                            let _ = tx.send(None);
                            return;
                        }
                    };
                    let _ = tx.send(Some(server.socket_path().to_path_buf()));
                    if let Err(e) = server.serve().await {
                        tracing::warn!(err = %e, "kanshou server exited with error");
                    }
                });
            });

        if let Err(e) = spawned {
            // THE DIVERGENCE, RESOLVED. This was `.expect()` in two of the
            // three copies. A sidecar that cannot start is not a reason to
            // kill the application it is meant to observe.
            tracing::warn!(
                app = app_name,
                err = %e,
                "could not spawn the kanshou thread; introspection disabled",
            );
            return None;
        }

        // The thread always sends exactly once before serving, so this is a
        // bounded wait. A RecvError means the thread died before reporting —
        // treated as "no introspection", never as a reason to fail.
        rx.recv().unwrap_or(None)
    }

    /// Run the accept loop indefinitely. Each accepted connection is
    /// handled in its own tokio task — the loop returns to accept the
    /// next client without blocking. Errors during accept are
    /// logged via `tracing::warn!` and the loop continues.
    pub async fn serve(self) -> std::io::Result<()> {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let state = Arc::clone(&self.state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, state).await {
                            tracing::warn!(error = ?e, "kanshou connection ended with error");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "kanshou accept failed");
                    // Brief backoff so a runaway error doesn't pin a core.
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
    }
}

impl<T: Introspect + 'static> Drop for Server<T> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Per-connection loop. Reads length-prefixed JSON queries, dispatches
/// against the state, writes length-prefixed JSON results. Returns
/// `Ok(())` on clean EOF and `Err` on protocol or I/O failure.
async fn handle_connection<T: Introspect>(
    mut stream: UnixStream,
    state: Arc<T>,
) -> std::io::Result<()> {
    loop {
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        // 4 MiB cap per frame — far above any reasonable query, well
        // below allocator stress. Tighter than serde_json's default.
        if len > 4 * 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("kanshou query frame too large: {len} bytes"),
            ));
        }
        let mut req_buf = vec![0u8; len];
        stream.read_exact(&mut req_buf).await?;

        let result: QueryResult = serde_json::from_slice::<Query>(&req_buf)
            .map_err(|e| QueryError::internal(format!("bad query JSON: {e}")))
            .and_then(|q| state.query(&q));

        let resp_bytes = serde_json::to_vec(&result).unwrap_or_else(|e| {
            let err: QueryResult = Err(QueryError::internal(format!(
                "kanshou response serialization failed: {e}"
            )));
            serde_json::to_vec(&err).expect("error envelope serialization is infallible")
        });

        stream
            .write_all(&u32::try_from(resp_bytes.len()).unwrap_or(u32::MAX).to_be_bytes())
            .await?;
        stream.write_all(&resp_bytes).await?;
        stream.flush().await?;
    }
}

#[cfg(test)]
mod sidecar_tests {
    use super::*;
    use crate::types::{Query, QueryError, QueryResult};

    struct Probe;
    impl Introspect for Probe {
        fn query(&self, _q: &Query) -> QueryResult {
            Err(QueryError::unknown_method("probe"))
        }
    }

    /// **THE CONTRACT, AND THE DIVERGENCE IT RESOLVES.**
    ///
    /// A sidecar that cannot start must never take the application with it.
    /// Two of the three hand-rolled copies this method replaced ended their
    /// spawn with `.expect(...)` while their own doc comment promised
    /// best-effort degradation — so under thread-spawn `EAGAIN` a missing
    /// debug socket killed the terminal and the multiplexer daemon.
    ///
    /// There is no arm of `spawn_sidecar` that can panic. This exercises the
    /// success path; the failure paths are unreachable without exhausting
    /// threads, and are `return None` by construction rather than by policy.
    #[test]
    fn spawning_a_sidecar_returns_a_bound_path_and_never_panics() {
        let app = format!("kanshou-test-{}", std::process::id());
        let path = Server::spawn_sidecar(&app, Arc::new(Probe));
        let path = path.expect("bind should succeed in a test environment");
        // The bind happened on THIS thread before the spawn, so a returned
        // path means the socket exists — not merely that a thread started.
        assert!(
            path.exists(),
            "spawn_sidecar returned {} but nothing is there — the path must \
             be bound before it is handed back, because callers announce it \
             as `introspection live`",
            path.display(),
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Two apps do not collide: the socket path carries the app name and pid.
    #[test]
    fn two_apps_get_distinct_sockets() {
        let a = Server::spawn_sidecar(&format!("kt-a-{}", std::process::id()), Arc::new(Probe));
        let b = Server::spawn_sidecar(&format!("kt-b-{}", std::process::id()), Arc::new(Probe));
        let (a, b) = (a.expect("a binds"), b.expect("b binds"));
        assert_ne!(a, b);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }
}
