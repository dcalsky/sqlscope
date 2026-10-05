//! A JSON ABI over sqlscope for WebAssembly hosts.
//!
//! The host allocates input buffers with [`sqlscope_alloc`], calls
//! [`sqlscope_call`] with an operation name and a JSON request, reads the
//! JSON response located by the returned `(pointer << 32) | length`, and
//! releases every buffer with [`sqlscope_free`].
//!
//! A response is either `{"ok": <result>}` or
//! `{"error": {"kind": "<kind>", "message": "<message>"}}` where kind is one
//! of `invalid_argument`, `parse`, `unsupported` or `internal`.

use std::collections::BTreeMap;

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

/// Executes one request and returns the JSON response. Exposed for tests and
/// non-wasm hosts.
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
// WebAssembly exports
// ---------------------------------------------------------------------------

/// The ABI version implemented by this module.
#[no_mangle]
pub extern "C" fn sqlscope_abi_version() -> u32 {
    ABI_VERSION
}

/// Allocates `len` bytes in module memory for the host to fill.
#[no_mangle]
pub extern "C" fn sqlscope_alloc(len: u32) -> *mut u8 {
    let mut buffer = Vec::<u8>::with_capacity(len as usize);
    let pointer = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    pointer
}

/// Releases a buffer returned by [`sqlscope_alloc`] or [`sqlscope_call`].
///
/// # Safety
/// `pointer` and `len` must describe a live buffer from this module.
#[no_mangle]
pub unsafe extern "C" fn sqlscope_free(pointer: *mut u8, len: u32) {
    if !pointer.is_null() {
        drop(Vec::from_raw_parts(pointer, 0, len as usize));
    }
}

/// Runs `operation` on the JSON `request` and returns the response buffer as
/// `(pointer << 32) | length`.
///
/// # Safety
/// The pointer/length pairs must describe live, initialized buffers.
#[no_mangle]
pub unsafe extern "C" fn sqlscope_call(op: *const u8, op_len: u32, request: *const u8, request_len: u32) -> u64 {
    let operation = std::slice::from_raw_parts(op, op_len as usize);
    let request = std::slice::from_raw_parts(request, request_len as usize);
    let response = match std::str::from_utf8(operation) {
        Ok(operation) => execute(operation, request),
        Err(_) => invalid("operation name is not UTF-8".to_owned()),
    };
    let mut bytes = serde_json::to_vec(&response).expect("serializable response");
    bytes.shrink_to_fit();
    let len = bytes.len() as u64;
    let pointer = bytes.as_mut_ptr() as u64;
    std::mem::forget(bytes);
    (pointer << 32) | len
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
}
