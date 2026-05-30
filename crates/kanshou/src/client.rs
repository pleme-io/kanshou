//! Discovery + client. Walk the socket directory to enumerate every
//! running kanshou consumer on this host; open a connection to one
//! and ship queries through it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::path::{parse_socket_name, socket_dir};
use crate::types::{Query, QueryResult};

/// A live kanshou consumer the discovery walk turned up. `pid`
/// liveness is NOT verified here — callers that care
/// (e.g. operator tools) re-check via `kill(pid, 0)` or
/// `/proc/<pid>` before connecting. Stale sockets get filtered when
/// the connect attempt fails with `ECONNREFUSED`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredInstance {
    pub app_name: String,
    pub pid: u32,
    pub socket_path: PathBuf,
}

/// Enumerate every kanshou socket in the canonical directory. Pass
/// `Some(app_name)` to filter; `None` returns all.
///
/// Order is dirent-order — callers that want deterministic ordering
/// sort by `app_name` or `pid`. Returns an empty vec when the
/// directory doesn't exist (no consumers on this host yet).
#[must_use]
pub fn discover(app_name: Option<&str>) -> Vec<DiscoveredInstance> {
    let dir = socket_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let name_str = name.to_str()?;
            let (app, pid) = parse_socket_name(name_str)?;
            Some(DiscoveredInstance {
                app_name: app,
                pid,
                socket_path: e.path(),
            })
        })
        .filter(|i| app_name.is_none_or(|filter| i.app_name == filter))
        .collect()
}

/// Connection to a running kanshou server. Stays open across queries
/// — the consumer can fire many before dropping.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Open a connection to the socket at `path`. Returns the same
    /// IO error `UnixStream::connect` does on failure (typically
    /// `ECONNREFUSED` when the process died and left a stale socket
    /// — callers can use that signal to prune the discovery list).
    pub async fn connect(path: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self { stream })
    }

    /// Ship a single query and read back the typed result.
    /// Length-prefixed JSON in both directions.
    pub async fn query(&mut self, q: &Query) -> std::io::Result<QueryResult> {
        let req = serde_json::to_vec(q)?;
        self.stream
            .write_all(
                &u32::try_from(req.len())
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "query frame too large",
                        )
                    })?
                    .to_be_bytes(),
            )
            .await?;
        self.stream.write_all(&req).await?;
        self.stream.flush().await?;

        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 4 * 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("response frame too large: {len} bytes"),
            ));
        }
        let mut resp = vec![0u8; len];
        self.stream.read_exact(&mut resp).await?;
        serde_json::from_slice(&resp)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}
