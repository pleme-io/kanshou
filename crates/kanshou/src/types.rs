//! Typed wire schema — the contract every kanshou server speaks and
//! every client consumes. Stable serde shape.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A typed introspection query. A path of field/method names walks
/// into the consumer's `AppState`. `args` parameterizes a method call
/// when the leaf is invocable; empty for plain field reads.
///
/// Examples:
/// - `Query { path: vec!["sessions"], args: vec![] }` — read a field
/// - `Query { path: vec!["queue", "depth"], args: vec![] }` — nested
/// - `Query { path: vec!["snapshot_grid"], args: vec![json!("sid1")] }` — method
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Query {
    pub path: Vec<String>,
    #[serde(default)]
    pub args: Vec<serde_json::Value>,
}

impl Query {
    /// Construct a plain field read.
    #[must_use]
    pub fn field(path: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            path: path.into_iter().map(Into::into).collect(),
            args: vec![],
        }
    }

    /// Construct a method invocation.
    #[must_use]
    pub fn call(
        path: impl IntoIterator<Item = impl Into<String>>,
        args: impl IntoIterator<Item = serde_json::Value>,
    ) -> Self {
        Self {
            path: path.into_iter().map(Into::into).collect(),
            args: args.into_iter().collect(),
        }
    }
}

/// Typed query result. JSON value on success; typed error on miss
/// or shape mismatch.
pub type QueryResult = Result<serde_json::Value, QueryError>;

/// Typed errors every consumer returns. Stable serde shape so
/// clients can match on the variant without parsing strings.
#[derive(Debug, Clone, Serialize, Deserialize, Error, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum QueryError {
    #[error("unknown field: {field}")]
    UnknownField { field: String },
    #[error("unknown method: {method}")]
    UnknownMethod { method: String },
    #[error("type mismatch at {path}: expected {expected}, got {actual}")]
    TypeMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("argument count mismatch: expected {expected}, got {actual}")]
    BadArity { expected: usize, actual: usize },
    #[error("internal error: {reason}")]
    Internal { reason: String },
}

impl QueryError {
    #[must_use]
    pub fn unknown_field(field: impl Into<String>) -> Self {
        Self::UnknownField {
            field: field.into(),
        }
    }
    #[must_use]
    pub fn unknown_method(method: impl Into<String>) -> Self {
        Self::UnknownMethod {
            method: method.into(),
        }
    }
    #[must_use]
    pub fn internal(reason: impl Into<String>) -> Self {
        Self::Internal {
            reason: reason.into(),
        }
    }
}

/// The trait every consumer's `AppState` implements to expose its
/// queryable surface. `Send + Sync` required so the `Arc<T>` is
/// shareable across tokio tasks.
///
/// Consumers can implement by hand for fine control, or (when
/// `#[derive(Introspect)]` ships in `gen-macros` — phase 2) derive
/// it: every `pub` `Serialize` field becomes queryable by name, every
/// `pub` `&self` method becomes a method-call leaf.
pub trait Introspect: Send + Sync {
    /// Dispatch a typed query against the receiver. Returns a JSON
    /// value on success or a typed [`QueryError`] on miss.
    fn query(&self, q: &Query) -> QueryResult;

    /// Returns the top-level queryable surface as a static string
    /// slice. Used by operator tools to enumerate what's available
    /// without trial-and-error queries. Default: empty (consumers
    /// override).
    fn schema(&self) -> &'static [&'static str] {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_field_constructor() {
        let q = Query::field(["sessions"]);
        assert_eq!(q.path, vec!["sessions".to_string()]);
        assert!(q.args.is_empty());
    }

    #[test]
    fn query_call_constructor() {
        let q = Query::call(["snapshot_grid"], [serde_json::json!("sid1")]);
        assert_eq!(q.path, vec!["snapshot_grid".to_string()]);
        assert_eq!(q.args, vec![serde_json::json!("sid1")]);
    }

    #[test]
    fn query_serde_roundtrip() {
        let q = Query::call(["nested", "field"], [serde_json::json!(42)]);
        let json = serde_json::to_string(&q).unwrap();
        let parsed: Query = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, q);
    }

    #[test]
    fn query_error_serde() {
        let e = QueryError::unknown_field("foo");
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("unknown-field"));
        assert!(json.contains("foo"));
        let parsed: QueryError = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, e);
    }

    #[test]
    fn query_error_display() {
        assert_eq!(
            QueryError::unknown_field("bar").to_string(),
            "unknown field: bar"
        );
        assert_eq!(
            QueryError::TypeMismatch {
                path: "a.b".into(),
                expected: "u64".into(),
                actual: "string".into(),
            }
            .to_string(),
            "type mismatch at a.b: expected u64, got string"
        );
    }
}
