//! MCP-forwarding helper. The canonical shape for a stdio MCP
//! server that needs to introspect a separate GUI / daemon process:
//! discover the live consumer over kanshou, forward the query, fall
//! back to a local implementation when no consumer is running.
//!
//! Closes the "MCP returns process-local zeros while the live
//! consumer renders" class with a single one-line call per tool.

use std::path::Path;

use crate::client::{discover, Client, DiscoveredInstance};
use crate::types::{Query, QueryError, QueryResult};

/// Discover the most-recent live consumer of `app_name`, ship the
/// query through kanshou, return the typed result. When no consumer
/// is running, `fallback` is invoked — pass `|| Err(...)` for tools
/// that genuinely require a live process, or a sensible local default
/// for tools that work either way.
///
/// "Most recent" = highest PID, which on macOS and Linux is a
/// reasonable proxy for "most recently launched". Operators wanting
/// a specific PID set `MADO_TARGET_PID` (or analogous) and the
/// `forward_to_pid` variant.
pub async fn forward<F>(app_name: &str, q: &Query, fallback: F) -> QueryResult
where
    F: FnOnce() -> QueryResult,
{
    // Try EVERY discovered socket in most-recent-first order, not just the
    // single newest. Sockets outlive their processes (a crashed/killed GUI
    // leaves its mado-<pid>.sock behind), so single-pick meant one stale
    // socket hid every LIVE instance behind it — the MCP server reported
    // {"count":0} with a healthy GUI running (incident 2026-06-10; found
    // while building the L2 e2e harness, which depends on this path).
    let mut all = discover(Some(app_name));
    all.sort_by_key(|i| std::cmp::Reverse(i.pid));
    for target in all {
        match connect_and_query(&target.socket_path, q).await {
            Ok(result) => return result,
            Err(e) => {
                tracing::debug!(
                    target = %target.app_name,
                    pid = target.pid,
                    error = ?e,
                    "kanshou socket unreachable (stale?); trying next"
                );
            }
        }
    }
    fallback()
}

/// Like [`forward`] but targets a specific PID. Returns the fallback
/// when that PID has no live kanshou socket.
pub async fn forward_to_pid<F>(app_name: &str, pid: u32, q: &Query, fallback: F) -> QueryResult
where
    F: FnOnce() -> QueryResult,
{
    let target = discover(Some(app_name))
        .into_iter()
        .find(|inst| inst.pid == pid);
    match target {
        Some(t) => match connect_and_query(&t.socket_path, q).await {
            Ok(result) => result,
            Err(_) => fallback(),
        },
        None => fallback(),
    }
}

async fn connect_and_query(socket: &Path, q: &Query) -> std::io::Result<QueryResult> {
    let mut client = Client::connect(socket).await?;
    client.query(q).await
}

/// Result of [`forward_status`] — combines the kanshou wire result
/// with a tag indicating which path the data came from. Useful when
/// MCP tools want to surface "this is process-local fallback, not
/// the live consumer's state" in the response.
pub enum ForwardOutcome {
    Live { pid: u32, value: serde_json::Value },
    Fallback { value: serde_json::Value },
    LiveError { pid: u32, error: QueryError },
}

/// Like [`forward`] but returns a tagged outcome so the caller can
/// embed provenance in its response. Useful for MCP tools where the
/// agent operator wants to know whether the data is live or stale.
pub async fn forward_status<F>(
    app_name: &str,
    q: &Query,
    fallback: F,
) -> ForwardOutcome
where
    F: FnOnce() -> QueryResult,
{
    // Same stale-socket retry discipline as [`forward`]: walk every
    // discovered socket newest-first; the first that CONNECTS wins.
    let mut all = discover(Some(app_name));
    all.sort_by_key(|i| std::cmp::Reverse(i.pid));
    for target in all {
        match connect_and_query(&target.socket_path, q).await {
            Ok(Ok(value)) => {
                return ForwardOutcome::Live {
                    pid: target.pid,
                    value,
                }
            }
            Ok(Err(error)) => {
                return ForwardOutcome::LiveError {
                    pid: target.pid,
                    error,
                }
            }
            Err(e) => {
                tracing::debug!(
                    pid = target.pid,
                    error = ?e,
                    "kanshou socket unreachable (stale?); trying next"
                );
            }
        }
    }
    match fallback() {
        Ok(value) => ForwardOutcome::Fallback { value },
        Err(error) => ForwardOutcome::LiveError {
            pid: 0,
            error,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    struct State;
    impl crate::Introspect for State {
        fn query(&self, q: &crate::Query) -> crate::QueryResult {
            match q.path.as_slice() {
                [s] if s == "ok" => Ok(serde_json::json!("live")),
                _ => Err(QueryError::unknown_field(q.path.join("."))),
            }
        }
    }

    static APP: AtomicU64 = AtomicU64::new(0);

    fn fresh_app_name() -> String {
        format!(
            "kanshou-mcp-test-{}-{}",
            std::process::id(),
            APP.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        )
    }

    #[tokio::test]
    async fn forward_hits_live_consumer() {
        let app = fresh_app_name();
        let server = crate::Server::new(&app, Arc::new(State)).unwrap();
        let server_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let result = forward(&app, &Query::field(["ok"]), || {
            Ok(serde_json::json!("fallback"))
        })
        .await;
        assert_eq!(result.unwrap(), serde_json::json!("live"));

        server_task.abort();
    }

    #[tokio::test]
    async fn forward_uses_fallback_when_no_consumer() {
        let app = fresh_app_name();
        let result = forward(&app, &Query::field(["ok"]), || {
            Ok(serde_json::json!("fallback"))
        })
        .await;
        assert_eq!(result.unwrap(), serde_json::json!("fallback"));
    }
}
