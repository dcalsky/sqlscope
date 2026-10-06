//! Protocol tests ported from sql-guard's native runtime
//! (`internal/nativeffi/execute_test.go`, `packages/go/integration_test.go`).
//! sql-guard wrapped each request in `{"abiVersion", "operation", "args"}`
//! and answered with a numeric status; sqlscope takes the operation name
//! separately and answers `{"ok": ...}` or `{"error": {"kind", "message"}}`,
//! and checks the ABI version when the library is loaded instead of per call.

use serde_json::Value;
use sqlscope_json::execute;

fn ok(operation: &str, request: &str) -> Value {
    let response = execute(operation, request.as_bytes());
    match response.get("ok") {
        Some(value) => value.clone(),
        None => panic!("{operation} failed: {response}"),
    }
}

fn error_kind(operation: &str, request: &str) -> String {
    let response = execute(operation, request.as_bytes());
    let error = response
        .get("error")
        .unwrap_or_else(|| panic!("{operation} succeeded: {response}"));
    assert!(
        error["message"].as_str().is_some_and(|m| !m.is_empty()),
        "error response has no message: {response}"
    );
    error["kind"].as_str().unwrap().to_owned()
}

#[test]
fn execute_all_operations() {
    let cases = [
        (
            "apply row filter with default dialect",
            "apply_row_filter",
            r#"{"sql":"SELECT id FROM orders","predicate":"tenant_id = 7"}"#,
            &["WHERE", "tenant_id"][..],
        ),
        (
            "bind CTEs",
            "inject_ctes",
            r#"{"sql":"SELECT id FROM recent","ctes":[{"name":"recent","query":"SELECT id FROM orders"}],"options":{"dialect":"trino"}}"#,
            &["WITH", "recent", "orders"],
        ),
        (
            "lineage source columns",
            "column_origins",
            r#"{"sql":"SELECT id FROM orders","options":{"dialect":"trino","schema":{"orders":["id"]}}}"#,
            &[r#""orders""#, r#""id""#],
        ),
        (
            "parse columns",
            "output_columns",
            r#"{"sql":"SELECT id, amount AS total FROM orders","options":{"dialect":"trino"}}"#,
            &[r#""id""#, r#""total""#],
        ),
        (
            "referenced columns",
            "referenced_columns",
            r#"{"sql":"SELECT id FROM orders WHERE status = 'PAID'","options":{"dialect":"trino"}}"#,
            &[r#""orders""#, r#""id""#, r#""status""#],
        ),
        (
            "referenced column usages",
            "column_usages",
            r#"{"sql":"SELECT id FROM orders WHERE status = 'PAID'","options":{"dialect":"trino"}}"#,
            &[r#""table":"orders""#, r#""clause":"SELECT""#, r#""clause":"WHERE""#],
        ),
        (
            "rewrite table references",
            "rewrite_tables",
            r#"{"sql":"SELECT id FROM sales.orders","options":{"dialect":"trino"},"rewrites":[{"matchKey":"sales.orders","inline":{"schema":"archive","table":"orders"}}]}"#,
            &["archive", "orders"],
        ),
        (
            "rewrite table references with union",
            "rewrite_tables",
            r#"{"sql":"SELECT id FROM sales.orders","options":{"dialect":"trino"},"rewrites":[{"matchKey":"sales.orders","union":{"tableAlias":"orders","columns":["id"],"branches":[{"schema":"shard1","table":"orders"},{"schema":"shard2","table":"orders"}]}}]}"#,
            &["UNION", "shard1", "shard2"],
        ),
    ];
    for (name, operation, request, fragments) in cases {
        let data = ok(operation, request).to_string();
        for fragment in fragments {
            assert!(data.contains(fragment), "{name}: {data} does not contain {fragment:?}");
        }
    }
}

#[test]
fn native_typed_operations() {
    assert_eq!(
        ok(
            "column_origins",
            r#"{"sql":"SELECT id FROM orders","options":{"dialect":"trino","schema":{"orders":["id"]}}}"#
        ),
        serde_json::json!({"orders": ["id"]})
    );
    assert_eq!(
        ok("output_columns", r#"{"sql":"SELECT id, amount AS total FROM orders"}"#),
        serde_json::json!(["id", "total"])
    );
    assert_eq!(
        ok(
            "referenced_columns",
            r#"{"sql":"SELECT id FROM orders WHERE status = 'PAID'"}"#
        ),
        serde_json::json!({"orders": ["id", "status"]})
    );
    assert_eq!(error_kind("output_columns", r#"{"sql":"SELECT ("}"#), "parse");
}

#[test]
fn execute_error_contract() {
    let cases = [
        ("empty", "output_columns", "", "invalid_argument"),
        ("malformed JSON", "output_columns", "not-json", "invalid_argument"),
        ("missing sql", "output_columns", "{}", "invalid_argument"),
        (
            "unknown operation",
            "unknown",
            r#"{"sql":"SELECT 1"}"#,
            "invalid_argument",
        ),
        ("invalid args", "output_columns", r#"{"sql":7}"#, "invalid_argument"),
        (
            "unknown field",
            "output_columns",
            r#"{"sql":"SELECT 1","bogus":1}"#,
            "invalid_argument",
        ),
        (
            "parse error",
            "output_columns",
            r#"{"sql":"SELECT (","options":{"dialect":"trino"}}"#,
            "parse",
        ),
        (
            "unsupported",
            "apply_row_filter",
            r#"{"sql":"UPDATE t SET a = 1","predicate":"a = 1","options":{"dialect":"mysql"}}"#,
            "unsupported",
        ),
    ];
    for (name, operation, request, kind) in cases {
        assert_eq!(error_kind(operation, request), kind, "{name}");
    }
}

#[test]
fn execute_concurrent() {
    let request = r#"{"sql":"SELECT id FROM orders WHERE status = 'PAID'","options":{"dialect":"trino"}}"#;
    std::thread::scope(|scope| {
        for _ in 0..32 {
            scope.spawn(|| {
                for _ in 0..50 {
                    ok("referenced_columns", request);
                }
            });
        }
    });
}
