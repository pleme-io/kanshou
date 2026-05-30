//! Verify `#[derive(Introspect)]` end-to-end: plain fields, atomic
//! `#[introspect(load)]`, nested `#[introspect(nested)]`,
//! `#[introspect(skip)]`, and `#[introspect(name = "...")]` rename.

#![cfg(feature = "derive")]

use std::sync::atomic::{AtomicU64, Ordering};

use kanshou::{Introspect, Query, QueryError};

#[derive(Introspect, serde::Serialize)]
struct InnerConfig {
    pub shell: String,
    pub width: u32,
}

#[derive(Introspect)]
struct AppState {
    pub sessions: Vec<String>,
    #[introspect(load)]
    pub frame_count: AtomicU64,
    #[introspect(nested)]
    pub config: InnerConfig,
    #[allow(dead_code)]
    #[introspect(skip)]
    pub internal: u32,
    #[introspect(name = "wire-name")]
    pub renamed_field: u32,
}

#[test]
fn derive_field_read() {
    let state = AppState {
        sessions: vec!["a".into(), "b".into()],
        frame_count: AtomicU64::new(0),
        config: InnerConfig {
            shell: "blzsh".into(),
            width: 80,
        },
        internal: 9999,
        renamed_field: 7,
    };
    let v = state.query(&Query::field(["sessions"])).unwrap();
    assert_eq!(v, serde_json::json!(["a", "b"]));
}

#[test]
fn derive_atomic_load() {
    let state = AppState {
        sessions: vec![],
        frame_count: AtomicU64::new(42),
        config: InnerConfig {
            shell: "blzsh".into(),
            width: 80,
        },
        internal: 0,
        renamed_field: 7,
    };
    let v = state.query(&Query::field(["frame_count"])).unwrap();
    assert_eq!(v, serde_json::json!(42));

    state.frame_count.store(99, Ordering::Relaxed);
    let v2 = state.query(&Query::field(["frame_count"])).unwrap();
    assert_eq!(v2, serde_json::json!(99));
}

#[test]
fn derive_nested_walk() {
    let state = AppState {
        sessions: vec![],
        frame_count: AtomicU64::new(0),
        config: InnerConfig {
            shell: "frostmourne".into(),
            width: 132,
        },
        internal: 0,
        renamed_field: 7,
    };
    // ["config"] alone → whole subtree
    let whole = state.query(&Query::field(["config"])).unwrap();
    assert_eq!(
        whole,
        serde_json::json!({ "shell": "frostmourne", "width": 132 })
    );
    // ["config", "shell"] → walks into the nested Introspect
    let inner = state
        .query(&Query::field(["config", "shell"]))
        .unwrap();
    assert_eq!(inner, serde_json::json!("frostmourne"));
}

#[test]
fn derive_skip_hidden() {
    let state = AppState {
        sessions: vec![],
        frame_count: AtomicU64::new(0),
        config: InnerConfig {
            shell: "blzsh".into(),
            width: 80,
        },
        internal: 9999,
        renamed_field: 7,
    };
    let err = state.query(&Query::field(["internal"])).unwrap_err();
    assert_eq!(err, QueryError::unknown_field("internal"));
}

#[test]
fn derive_rename() {
    let state = AppState {
        sessions: vec![],
        frame_count: AtomicU64::new(0),
        config: InnerConfig {
            shell: "blzsh".into(),
            width: 80,
        },
        internal: 0,
        renamed_field: 7,
    };
    // Querying the Rust name fails (renamed away).
    let err = state
        .query(&Query::field(["renamed_field"]))
        .unwrap_err();
    assert!(matches!(err, QueryError::UnknownField { .. }));
    // The wire name resolves.
    let v = state.query(&Query::field(["wire-name"])).unwrap();
    assert_eq!(v, serde_json::json!(7));
}

#[test]
fn derive_schema_lists_visible_fields() {
    let state = AppState {
        sessions: vec![],
        frame_count: AtomicU64::new(0),
        config: InnerConfig {
            shell: "blzsh".into(),
            width: 80,
        },
        internal: 0,
        renamed_field: 7,
    };
    let schema = state.schema();
    assert!(schema.contains(&"sessions"));
    assert!(schema.contains(&"frame_count"));
    assert!(schema.contains(&"config"));
    assert!(schema.contains(&"wire-name"));
    assert!(
        !schema.contains(&"internal"),
        "skip'd fields must not surface in schema"
    );
    assert!(
        !schema.contains(&"renamed_field"),
        "rename masks the Rust name"
    );
}
