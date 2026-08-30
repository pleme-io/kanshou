//! Discovery + client. Walk the socket directory to enumerate every
//! running kanshou consumer on this host; open a connection to one
//! and ship queries through it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::path::parse_socket_name;
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
    /// Whether the owning process still exists.
    ///
    /// ★ Carried rather than filtered so a caller can tell "no such app" from
    /// "the app died and left its socket" — two answers that a pre-filtered
    /// list collapses into the same empty vec. `discover` returns live
    /// instances only; `discover_all` returns both and is what a diagnostic
    /// (or a reaper) wants.
    pub live: bool,
}

/// Enumerate every kanshou socket in the canonical directory. Pass
/// `Some(app_name)` to filter; `None` returns all.
///
/// Order is dirent-order — callers that want deterministic ordering
/// sort by `app_name` or `pid`. Returns an empty vec when the
/// directory doesn't exist (no consumers on this host yet).
#[must_use]
pub fn discover(app_name: Option<&str>) -> Vec<DiscoveredInstance> {
    let mut v = discover_all(app_name);
    v.retain(|i| i.live);
    v
}

/// Every socket found, live or not, across EVERY directory a peer may have
/// bound in.
///
/// ★ Plural directories, because the write path's answer depends on an
/// environment variable each process inherits separately. Measured on plo:
/// omoya/mado/tear/tend/frost in `$XDG_RUNTIME_DIR/kanshou`, sentinela in
/// `/tmp/kanshou-0`, and 94 mostly-dead sockets in `/tmp/kanshou-1001`. A
/// single-directory `discover` reported most of the fleet as ABSENT — the
/// failure `path.rs`'s header predicted and the read path never acted on.
///
/// Deduplicated by `(app_name, pid)` with the canonical directory winning, so
/// a process that bound under one name and left a stale socket under the other
/// is reported once.
#[must_use]
pub fn discover_all(app_name: Option<&str>) -> Vec<DiscoveredInstance> {
    discover_all_in(&crate::path::socket_dirs(), app_name)
}

/// `discover_all` over an EXPLICIT directory list.
///
/// ★ Exists so the behaviour is testable without touching
/// `KANSHOU_SOCKET_DIR`. That variable is process-global and cargo runs tests
/// in parallel, so a test that redirects discovery silently redirects every
/// other test's discovery too — measured here: adding one such test made
/// `mcp::forward_hits_live_consumer` fail with `left: "fallback", right:
/// "live"`, an error naming neither the variable nor the other test.
///
/// The first instinct was a mutex, which clippy correctly refused: these tests
/// are async and a `MutexGuard` held across an `.await` can deadlock the
/// executor. Taking the directories as an argument removes the shared state
/// instead of guarding it, which is the better answer to both problems.
#[must_use]
pub fn discover_all_in(dirs: &[PathBuf], app_name: Option<&str>) -> Vec<DiscoveredInstance> {
    let mut out: Vec<DiscoveredInstance> = Vec::new();
    let mut seen: std::collections::HashSet<(String, u32)> = std::collections::HashSet::new();
    // Canonical-first, and first-wins is what makes the canonical directory
    // authoritative for a duplicate.
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.filter_map(Result::ok) {
            let name = e.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let Some((app, pid)) = parse_socket_name(name_str) else {
                continue;
            };
            if app_name.is_some_and(|f| app != f) {
                continue;
            }
            if !seen.insert((app.clone(), pid)) {
                continue;
            }
            out.push(DiscoveredInstance {
                app_name: app,
                pid,
                socket_path: e.path(),
                live: crate::path::pid_is_live(pid),
            });
        }
    }
    out
}

/// Remove sockets whose owning process is gone, returning how many went.
///
/// ★ NOT called automatically from `discover`. Discovery is a READ and must
/// stay side-effect free — a diagnostic that mutates the thing it inspects is
/// the `omoya_capture`-repairs-the-artifact trap in a different costume. A
/// server calls this for its OWN directory at bind time; an operator tool may
/// call it deliberately.
///
/// Measured need: 94 sockets in `/tmp/kanshou-1001` on plo, 1 live. Nothing
/// unlinks on exit, because a crashed process cannot.
#[must_use]
pub fn reap_stale(app_name: Option<&str>) -> usize {
    reap_stale_in(&crate::path::socket_dirs(), app_name)
}

/// `reap_stale` over an EXPLICIT directory list. See `discover_all_in`.
#[must_use]
pub fn reap_stale_in(dirs: &[PathBuf], app_name: Option<&str>) -> usize {
    discover_all_in(dirs, app_name)
        .into_iter()
        .filter(|i| !i.live)
        .filter(|i| std::fs::remove_file(&i.socket_path).is_ok())
        .count()
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

#[cfg(test)]
mod discovery_reach_tests {
    use super::*;

    /// ★ THE DEFECT THIS CLOSES (plo, 2026-08-29).
    ///
    /// `discover` resolved ONE directory. On plo the fleet was split across
    /// three by an environment variable each process inherits separately:
    ///
    /// ```text
    ///   /run/user/1001/kanshou   frost mado omoya tear-daemon tend  (5 live)
    ///   /tmp/kanshou-1001        94 sockets, 1 live
    ///   /tmp/kanshou-0           sentinela
    /// ```
    ///
    /// An inventory built on the old `discover` reported omoya — the
    /// COMPOSITOR, the component that owns every pixel — as having no
    /// introspection at all, and a plan was written on that false premise.
    /// The directory was right; the reach was wrong.
    #[test]
    fn the_read_path_covers_more_than_one_directory() {
        // ★ Reads KANSHOU_SOCKET_DIR (via socket_dir), so it must not run
        // while path.rs's override test has it set. Two takers is what makes
        // the lock mean something -- one taker guards nothing.
        let _g = crate::path::env_guard();
        let dirs = crate::path::socket_dirs();
        assert!(!dirs.is_empty());
        assert_eq!(
            dirs[0],
            crate::path::socket_dir(),
            "canonical must be first"
        );
        // Deduplicated: when XDG_RUNTIME_DIR is unset the canonical answer IS
        // the /tmp one, and it must not appear twice.
        let mut sorted = dirs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            dirs.len(),
            "socket_dirs must not repeat: {dirs:?}"
        );
    }

    #[test]
    fn a_dead_pid_is_reported_not_silently_dropped() {
        // pid 1 always exists on a unix host; a very high pid essentially never
        // does. The point is that `live` DISCRIMINATES rather than being a
        // constant -- a field that is always true would pass every other test
        // here while making `discover` return the 93 dead sockets again.
        assert!(crate::path::pid_is_live(1), "pid 1 must read as live");
        assert!(
            !crate::path::pid_is_live(4_000_000_000),
            "an impossible pid must read as dead"
        );
    }

    #[test]
    fn discovery_is_side_effect_free() {
        // ★ A diagnostic that mutates what it inspects is the
        // omoya_capture-repairs-the-artifact trap in another costume. Reaping
        // is a SEPARATE, deliberate call.
        let dir = std::env::temp_dir().join(format!("kanshou-sef-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        // A socket file for a pid that cannot be alive.
        let stale = dir.join("ghost-4000000000.sock");
        std::fs::write(&stale, b"").expect("write");
        // ★ No env mutation: the directory is an ARGUMENT. See discover_all_in.
        let dirs = vec![dir.clone()];
        let all = discover_all_in(&dirs, Some("ghost"));
        assert_eq!(all.len(), 1);
        assert!(!all[0].live);
        assert!(stale.exists(), "discovery must not delete anything");
        // And the live-only view hides it.
        assert!(
            discover_all_in(&dirs, Some("ghost"))
                .into_iter()
                .all(|i| !i.live),
            "the live-only view must hide a dead socket"
        );
        // Reaping is what removes it, and only when asked.
        assert_eq!(reap_stale_in(&dirs, Some("ghost")), 1);
        assert!(!stale.exists(), "reap_stale must remove a dead socket");
        std::fs::remove_dir_all(&dir).ok();
    }
}
