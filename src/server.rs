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
