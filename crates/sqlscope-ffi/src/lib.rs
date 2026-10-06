//! The C ABI of sqlscope, built as a shared and a static library.
//!
//! Every operation goes through one function: [`sqlscope_call`] takes an
//! operation name and a JSON request, both NUL-terminated UTF-8, and returns
//! a NUL-terminated JSON response that the caller releases with
//! [`sqlscope_free`]. The declarations live in `include/sqlscope.h`.
//!
//! A response is either `{"ok": <result>}` or
//! `{"error": {"kind": "<kind>", "message": "<message>"}}` where kind is one
//! of `invalid_argument`, `parse`, `unsupported` or `internal`. Every
//! function is safe to call concurrently from any thread.

use std::collections::BTreeMap;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlscope::{CteDef, Dialect, Error, ErrorKind, Options, RewriteTarget, TableRef, TableRewrite, UnionRewrite};

/// The ABI version; bump on incompatible changes to requests or responses.
pub const ABI_VERSION: u32 = 1;

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestOptions {
    dialect: Option<String>,
    schema: Option<BTreeMap<String, Vec<String>>>,
    table_names: Option<Vec<String>>,
    table_patterns: Option<Vec<String>>,
    default_db: Option<String>,
    strip_catalogs: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Table {
    table: String,
    schema: Option<String>,
    catalog: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Union {
    table_alias: String,
    columns: Vec<String>,
    branches: Vec<Table>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Rewrite {
    match_key: String,
    inline: Option<Table>,
    union: Option<Union>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cte {
    name: String,
    query: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    sql: String,
    predicate: Option<String>,
    #[serde(default)]
    ctes: Vec<Cte>,
    #[serde(default)]
    rewrites: Vec<Rewrite>,
    #[serde(default)]
    options: RequestOptions,
}

#[derive(Serialize)]
struct Usage {
    table: String,
    column: String,
    clause: &'static str,
}

impl From<Table> for TableRef {
    fn from(table: Table) -> Self {
        TableRef {
            table: table.table,
            schema: table.schema,
            catalog: table.catalog,
        }
    }
}

fn options(request: RequestOptions) -> Result<Options, Error> {
    let mut options = Options::new();
    if let Some(dialect) = request.dialect.filter(|d| !d.trim().is_empty()) {
        options = options.dialect(dialect.parse::<Dialect>()?);
    }
    if let Some(schema) = request.schema {
        options = options.schema(schema);
    }
    if let Some(names) = request.table_names {
        options = options.table_names(names);
    }
    if let Some(patterns) = request.table_patterns {
        options = options.table_patterns(patterns);
    }
    if let Some(db) = request.default_db {
        options = options.default_db(db);
    }
    if let Some(catalogs) = request.strip_catalogs {
        options = options.strip_catalogs(catalogs);
    }
    Ok(options)
}

fn invalid(message: String) -> Value {
    json!({"error": {"kind": "invalid_argument", "message": message}})
}

/// Executes one request and returns the JSON response.
pub fn execute(operation: &str, request: &[u8]) -> Value {
    let request: Request = match serde_json::from_slice(request) {
        Ok(request) => request,
        Err(error) => return invalid(format!("invalid request: {error}")),
    };
    let result = dispatch(operation, request);
    match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({"error": {"kind": kind(error.kind()), "message": error.message()}}),
    }
}

fn kind(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidArgument => "invalid_argument",
        ErrorKind::Parse => "parse",
        ErrorKind::Unsupported => "unsupported",
        _ => "internal",
    }
}

fn to_value<T: Serialize>(value: T) -> Result<Value, Error> {
    Ok(serde_json::to_value(value).expect("serializable result"))
}

fn dispatch(operation: &str, request: Request) -> Result<Value, Error> {
    let Request {
        sql,
        predicate,
        ctes,
        rewrites,
        options: request_options,
    } = request;
    let options = options(request_options)?;
    match operation {
        "apply_row_filter" => to_value(sqlscope::apply_row_filter(
            &sql,
            predicate.as_deref().unwrap_or_default(),
            &options,
        )?),
        "inject_ctes" => {
            let ctes: Vec<CteDef> = ctes.into_iter().map(|c| CteDef::new(c.name, c.query)).collect();
            to_value(sqlscope::inject_ctes(&sql, &ctes, &options)?)
        }
        "rewrite_tables" => {
            let mut plan = Vec::with_capacity(rewrites.len());
            for rewrite in rewrites {
                let target = match (rewrite.inline, rewrite.union) {
                    (Some(table), None) => RewriteTarget::Inline(table.into()),
                    (None, Some(union)) => RewriteTarget::Union(UnionRewrite {
                        table_alias: union.table_alias,
                        columns: union.columns,
                        branches: union.branches.into_iter().map(Into::into).collect(),
                    }),
                    _ => {
                        return Err(invalid_error(format!(
                            "table rewrite {:?} must have exactly one of inline or union",
                            rewrite.match_key
                        )))
                    }
                };
                plan.push(TableRewrite {
                    match_key: rewrite.match_key,
                    target,
                });
            }
            to_value(sqlscope::rewrite_tables(&sql, &plan, &options)?)
        }
        "column_origins" => to_value(sqlscope::column_origins(&sql, &options)?),
        "output_columns" => to_value(sqlscope::output_columns(&sql, &options)?),
        "referenced_columns" => to_value(sqlscope::referenced_columns(&sql, &options)?),
        "column_usages" => to_value(
            sqlscope::column_usages(&sql, &options)?
                .into_iter()
                .map(|usage| Usage {
                    table: usage.table,
                    column: usage.column,
                    clause: usage.clause.as_str(),
                })
                .collect::<Vec<_>>(),
        ),
        other => Err(invalid_error(format!("unknown operation {other:?}"))),
    }
}

fn invalid_error(message: String) -> Error {
    Error::new(ErrorKind::InvalidArgument, message)
}

// ---------------------------------------------------------------------------
// C exports
// ---------------------------------------------------------------------------

/// The ABI version implemented by this library.
#[no_mangle]
pub extern "C" fn sqlscope_abi_version() -> u32 {
    ABI_VERSION
}

/// The library version as a static NUL-terminated string; do not free it.
#[no_mangle]
pub extern "C" fn sqlscope_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Reads a NUL-terminated UTF-8 argument.
///
/// # Safety
/// `pointer` must be null or point to a NUL-terminated string.
unsafe fn argument<'a>(pointer: *const c_char, name: &str) -> Result<&'a str, Value> {
    if pointer.is_null() {
        return Err(invalid(format!("{name} is NULL")));
    }
    CStr::from_ptr(pointer)
        .to_str()
        .map_err(|_| invalid(format!("{name} is not UTF-8")))
}

/// Runs `operation` on the JSON `request` and returns the JSON response as a
/// NUL-terminated string owned by the caller; release it with
/// [`sqlscope_free`]. Never returns NULL.
///
/// # Safety
/// `operation` and `request` must be null or point to NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn sqlscope_call(operation: *const c_char, request: *const c_char) -> *mut c_char {
    let response = catch_unwind(AssertUnwindSafe(|| {
        let operation = argument(operation, "operation")?;
        let request = argument(request, "request")?;
        Ok(execute(operation, request.as_bytes()))
    }))
    .unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_owned());
        Ok(json!({"error": {"kind": "internal", "message": format!("panic: {message}")}}))
    })
    .unwrap_or_else(|error: Value| error);
    // serde_json escapes control characters, so the response has no NUL.
    let bytes = serde_json::to_vec(&response).expect("serializable response");
    CString::new(bytes).expect("JSON has no NUL").into_raw()
}

/// Releases a response returned by [`sqlscope_call`]. NULL is ignored.
///
/// # Safety
/// `response` must be null or a pointer returned by [`sqlscope_call`] that
/// has not been released yet.
#[no_mangle]
pub unsafe extern "C" fn sqlscope_free(response: *mut c_char) {
    if !response.is_null() {
        drop(CString::from_raw(response));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executes_requests() {
        let response = execute(
            "apply_row_filter",
            br#"{"sql": "SELECT id FROM orders", "predicate": "t = 1", "options": {"dialect": "postgres"}}"#,
        );
        assert_eq!(
            response,
            json!({"ok": "SELECT id FROM (SELECT * FROM orders WHERE t = 1) AS orders"})
        );

        let response = execute("column_usages", br#"{"sql": "SELECT a FROM t WHERE b > 1"}"#);
        assert_eq!(
            response,
            json!({"ok": [
                {"table": "t", "column": "a", "clause": "SELECT"},
                {"table": "t", "column": "b", "clause": "WHERE"},
            ]})
        );
        assert_eq!(
            execute("output_columns", br#"{"sql": "DROP TABLE t"}"#),
            json!({"ok": null})
        );
    }

    #[test]
    fn reports_errors() {
        let kind = |op: &str, request: &[u8]| execute(op, request)["error"]["kind"].clone();
        assert_eq!(kind("column_origins", br#"{"sql": "SELECT FROM WHERE"}"#), "parse");
        assert_eq!(kind("nope", br#"{"sql": "SELECT 1"}"#), "invalid_argument");
        assert_eq!(kind("column_origins", br#"{"sql": 1}"#), "invalid_argument");
        assert_eq!(
            kind("column_origins", br#"{"sql": "SELECT 1", "bogus": 1}"#),
            "invalid_argument"
        );
        assert_eq!(
            kind(
                "rewrite_tables",
                br#"{"sql": "SELECT 1", "rewrites": [{"matchKey": "s.t"}]}"#
            ),
            "invalid_argument"
        );
        assert_eq!(
            kind("apply_row_filter", br#"{"sql": "DELETE FROM t", "predicate": "x"}"#),
            "unsupported"
        );
    }

    #[test]
    fn c_abi_round_trip() {
        let call = |operation: *const c_char, request: *const c_char| unsafe {
            let response = sqlscope_call(operation, request);
            let value: Value = serde_json::from_slice(CStr::from_ptr(response).to_bytes()).unwrap();
            sqlscope_free(response);
            value
        };
        let op = c"output_columns";
        assert_eq!(
            call(op.as_ptr(), c"{\"sql\": \"SELECT a, b AS c FROM t\"}".as_ptr()),
            json!({"ok": ["a", "c"]})
        );
        assert_eq!(
            call(std::ptr::null(), c"{}".as_ptr())["error"]["kind"],
            "invalid_argument"
        );
        assert_eq!(call(op.as_ptr(), std::ptr::null())["error"]["kind"], "invalid_argument");
        assert_eq!(
            call(op.as_ptr(), c"{\"sql\": \"\xff\"}".as_ptr())["error"]["kind"],
            "invalid_argument"
        );
        unsafe { sqlscope_free(std::ptr::null_mut()) };

        let version = unsafe { CStr::from_ptr(sqlscope_version()) };
        assert_eq!(version.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
        assert_eq!(sqlscope_abi_version(), ABI_VERSION);
    }
}
