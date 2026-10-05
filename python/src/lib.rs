//! Native extension behind the `sqlscope` Python package.
//!
//! The functions here take plain Python values; the public, typed API lives
//! in `python/sqlscope/__init__.py`.

use std::collections::BTreeMap;

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;

use sqlscope::{CteDef, Dialect, ErrorKind, Options, RewriteTarget, TableRef, TableRewrite, UnionRewrite};

create_exception!(_native, Error, PyException, "Base class of every sqlscope error.");
create_exception!(
    _native,
    InvalidArgumentError,
    Error,
    "An option or argument is invalid."
);
create_exception!(_native, ParseError, Error, "The SQL text could not be parsed.");
create_exception!(
    _native,
    UnsupportedError,
    Error,
    "The statement shape is not supported, or the input exceeded a safety limit."
);
create_exception!(
    _native,
    InternalError,
    Error,
    "sqlscope produced an invalid result (a bug)."
);

fn to_py(error: sqlscope::Error) -> PyErr {
    let message = error.message().to_owned();
    match error.kind() {
        ErrorKind::InvalidArgument => InvalidArgumentError::new_err(message),
        ErrorKind::Parse => ParseError::new_err(message),
        ErrorKind::Unsupported => UnsupportedError::new_err(message),
        _ => InternalError::new_err(message),
    }
}

type Schema = BTreeMap<String, Vec<String>>;
/// `(table, schema, catalog)`.
type Table = (String, Option<String>, Option<String>);

fn options(dialect: Option<&str>) -> PyResult<Options> {
    let mut options = Options::new();
    if let Some(dialect) = dialect {
        options = options.dialect(dialect.parse::<Dialect>().map_err(to_py)?);
    }
    Ok(options)
}

fn table(value: Table) -> TableRef {
    TableRef {
        table: value.0,
        schema: value.1,
        catalog: value.2,
    }
}

/// Runs `operation` without holding the GIL.
fn run<T: Send>(py: Python<'_>, operation: impl FnOnce() -> sqlscope::Result<T> + Send) -> PyResult<T> {
    py.detach(operation).map_err(to_py)
}

#[pyfunction]
#[pyo3(signature = (sql, predicate, dialect=None, table_names=None, table_patterns=None, default_db=None))]
fn apply_row_filter(
    py: Python<'_>,
    sql: &str,
    predicate: &str,
    dialect: Option<&str>,
    table_names: Option<Vec<String>>,
    table_patterns: Option<Vec<String>>,
    default_db: Option<String>,
) -> PyResult<String> {
    let mut options = options(dialect)?;
    if let Some(names) = table_names {
        options = options.table_names(names);
    }
    if let Some(patterns) = table_patterns {
        options = options.table_patterns(patterns);
    }
    if let Some(db) = default_db {
        options = options.default_db(db);
    }
    run(py, || sqlscope::apply_row_filter(sql, predicate, &options))
}

#[pyfunction]
#[pyo3(signature = (sql, ctes, dialect=None))]
fn inject_ctes(py: Python<'_>, sql: &str, ctes: Vec<(String, String)>, dialect: Option<&str>) -> PyResult<String> {
    let options = options(dialect)?;
    let ctes: Vec<CteDef> = ctes.into_iter().map(|(name, query)| CteDef { name, query }).collect();
    run(py, || sqlscope::inject_ctes(sql, &ctes, &options))
}

/// One rewrite: `(match_key, inline_target, union)` where `union` is
/// `(table_alias, columns, branches)`. Exactly one target must be given.
type Rewrite = (String, Option<Table>, Option<(String, Vec<String>, Vec<Table>)>);

#[pyfunction]
#[pyo3(signature = (sql, rewrites, dialect=None, strip_catalogs=None))]
fn rewrite_tables(
    py: Python<'_>,
    sql: &str,
    rewrites: Vec<Rewrite>,
    dialect: Option<&str>,
    strip_catalogs: Option<Vec<String>>,
) -> PyResult<String> {
    let mut options = options(dialect)?;
    if let Some(catalogs) = strip_catalogs {
        options = options.strip_catalogs(catalogs);
    }
    let rewrites = rewrites
        .into_iter()
        .map(|(match_key, inline, union)| {
            let target = match (inline, union) {
                (Some(target), None) => RewriteTarget::Inline(table(target)),
                (None, Some((table_alias, columns, branches))) => RewriteTarget::Union(UnionRewrite {
                    table_alias,
                    columns,
                    branches: branches.into_iter().map(table).collect(),
                }),
                _ => {
                    return Err(InvalidArgumentError::new_err(format!(
                        "table rewrite {match_key:?} must have exactly one of inline or union"
                    )))
                }
            };
            Ok(TableRewrite { match_key, target })
        })
        .collect::<PyResult<Vec<_>>>()?;
    run(py, || sqlscope::rewrite_tables(sql, &rewrites, &options))
}

fn analysis_options(dialect: Option<&str>, schema: Option<Schema>) -> PyResult<Options> {
    Ok(options(dialect)?.schema(schema.unwrap_or_default()))
}

#[pyfunction]
#[pyo3(signature = (sql, dialect=None, schema=None))]
fn column_origins(
    py: Python<'_>,
    sql: &str,
    dialect: Option<&str>,
    schema: Option<Schema>,
) -> PyResult<BTreeMap<String, Vec<String>>> {
    let options = analysis_options(dialect, schema)?;
    run(py, || sqlscope::column_origins(sql, &options))
}

#[pyfunction]
#[pyo3(signature = (sql, dialect=None, schema=None))]
fn output_columns(
    py: Python<'_>,
    sql: &str,
    dialect: Option<&str>,
    schema: Option<Schema>,
) -> PyResult<Option<Vec<String>>> {
    let options = analysis_options(dialect, schema)?;
    run(py, || sqlscope::output_columns(sql, &options))
}

#[pyfunction]
#[pyo3(signature = (sql, dialect=None, schema=None))]
fn referenced_columns(
    py: Python<'_>,
    sql: &str,
    dialect: Option<&str>,
    schema: Option<Schema>,
) -> PyResult<BTreeMap<String, Vec<String>>> {
    let options = analysis_options(dialect, schema)?;
    run(py, || sqlscope::referenced_columns(sql, &options))
}

/// Returns `(table, column, clause)` triples.
#[pyfunction]
#[pyo3(signature = (sql, dialect=None, schema=None))]
fn column_usages(
    py: Python<'_>,
    sql: &str,
    dialect: Option<&str>,
    schema: Option<Schema>,
) -> PyResult<Vec<(String, String, &'static str)>> {
    let options = analysis_options(dialect, schema)?;
    let usages = run(py, || sqlscope::column_usages(sql, &options))?;
    Ok(usages
        .into_iter()
        .map(|usage| (usage.table, usage.column, usage.clause.as_str()))
        .collect())
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("Error", py.get_type::<Error>())?;
    module.add("InvalidArgumentError", py.get_type::<InvalidArgumentError>())?;
    module.add("ParseError", py.get_type::<ParseError>())?;
    module.add("UnsupportedError", py.get_type::<UnsupportedError>())?;
    module.add("InternalError", py.get_type::<InternalError>())?;
    module.add_function(wrap_pyfunction!(apply_row_filter, module)?)?;
    module.add_function(wrap_pyfunction!(inject_ctes, module)?)?;
    module.add_function(wrap_pyfunction!(rewrite_tables, module)?)?;
    module.add_function(wrap_pyfunction!(column_origins, module)?)?;
    module.add_function(wrap_pyfunction!(output_columns, module)?)?;
    module.add_function(wrap_pyfunction!(referenced_columns, module)?)?;
    module.add_function(wrap_pyfunction!(column_usages, module)?)?;
    Ok(())
}
