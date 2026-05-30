//! End-to-end test — spin up a server, connect a client, fire a few
//! queries, assert the typed results round-trip cleanly.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kanshou::path::socket_path;
use kanshou::{Client, Introspect, Query, QueryError, QueryResult, Server};

struct TestState {
    sessions: Vec<String>,
    frame_count: AtomicU64,
}

impl Introspect for TestState {
    fn query(&self, q: &Query) -> QueryResult {
        match q.path.as_slice() {
            [first] if first == "sessions" => {
                Ok(serde_json::to_value(&self.sessions).unwrap())
            }
            [first] if first == "frame_count" => Ok(serde_json::to_value(
                self.frame_count.load(Ordering::Relaxed),
            )
            .unwrap()),
            _ => Err(QueryError::unknown_field(q.path.join("."))),
        }
    }
    fn schema(&self) -> &'static [&'static str] {
        &["sessions", "frame_count"]
    }
}

#[tokio::test]
async fn end_to_end_query_roundtrip() {
    // Unique app name per test run so multiple runs don't collide on
    // the same socket path.
    let app_name = format!("kanshou-test-{}", std::process::id());
    let state = Arc::new(TestState {
        sessions: vec!["sid-a".into(), "sid-b".into()],
        frame_count: AtomicU64::new(42),
    });
    let server = Server::new(&app_name, Arc::clone(&state)).expect("bind");
    let path = server.socket_path().to_path_buf();
    let server_task = tokio::spawn(async move {
        let _ = server.serve().await;
    });

    // Brief pause so the listener is ready when the client connects.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let mut client = Client::connect(&path).await.expect("connect");

    let sessions = client
        .query(&Query::field(["sessions"]))
        .await
        .expect("query io")
        .expect("query ok");
    assert_eq!(
        sessions,
        serde_json::json!(["sid-a", "sid-b"]),
        "sessions field"
    );

    let frames = client
        .query(&Query::field(["frame_count"]))
        .await
        .expect("query io")
        .expect("query ok");
    assert_eq!(frames, serde_json::json!(42), "frame_count field");

    // Mutate the state and re-query — the server reads the live
    // Arc<T>, so a fresh value comes back without restarting.
    state.frame_count.store(99, Ordering::Relaxed);
    let frames2 = client
        .query(&Query::field(["frame_count"]))
        .await
        .expect("query io")
        .expect("query ok");
    assert_eq!(frames2, serde_json::json!(99), "frame_count updated");

    // Unknown field returns the typed error variant.
    let err = client
        .query(&Query::field(["nope"]))
        .await
        .expect("query io")
        .expect_err("err");
    assert_eq!(err, QueryError::unknown_field("nope"));

    // Drop the client cleanly; the server task is left running but
    // exits when this test process tears down.
    drop(client);
    server_task.abort();
}

#[tokio::test]
async fn socket_path_matches_canonical_layout() {
    let app_name = format!("kanshou-test-path-{}", std::process::id());
    let server = Server::new(&app_name, Arc::new(TestState {
        sessions: vec![],
        frame_count: AtomicU64::new(0),
    }))
    .expect("bind");
    let expected = socket_path(&app_name, std::process::id());
    assert_eq!(server.socket_path(), expected.as_path());
    drop(server);
}

#[tokio::test]
async fn discover_finds_the_running_server() {
    let app_name = format!("kanshou-test-disc-{}", std::process::id());
    let server = Server::new(&app_name, Arc::new(TestState {
        sessions: vec![],
        frame_count: AtomicU64::new(0),
    }))
    .expect("bind");
    let _path = server.socket_path().to_path_buf();
    let server_task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let instances = kanshou::discover(Some(&app_name));
    assert_eq!(instances.len(), 1, "exactly one instance");
    assert_eq!(instances[0].app_name, app_name);
    assert_eq!(instances[0].pid, std::process::id());

    server_task.abort();
}
