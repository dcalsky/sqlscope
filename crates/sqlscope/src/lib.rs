//! Scope-aware SQL analysis and rewriting on top of the
//! [polyglot](https://github.com/tobilg/polyglot) SQL engine.
//!
//! | Operation | Purpose |
//! | --- | --- |
//! | [`apply_row_filter`] | Filter every in-scope table by a predicate (row-level security). |
//! | [`inject_ctes`] | Prepend CTE definitions to a query's root `WITH`. |
//! | [`rewrite_tables`] | Replace table references with derived tables. |
//! | [`column_origins`] | Source columns whose values reach the result (column lineage). |
//! | [`output_columns`] | Names of the columns a statement outputs. |
//! | [`referenced_columns`] | Columns referenced anywhere, per table. |
//! | [`column_usages`] | Column references with the clause they appear in. |
//!
//! Every operation takes [`Options`]; operations are pure functions and safe
//! to call concurrently. Failures carry an [`ErrorKind`].

mod ast;
mod column_origins;
mod column_usages;
mod error;
mod inject_ctes;
mod normalize;
mod options;
mod output_columns;
mod resolver;
mod rewrite;
mod rewrite_tables;
mod row_filter;
mod schema;

pub use column_origins::column_origins;
pub use column_usages::{column_usages, referenced_columns, Clause, ColumnUsage};
pub use error::{Error, ErrorKind, Result};
pub use inject_ctes::{inject_ctes, CteDef};
pub use options::{Dialect, Options};
pub use output_columns::output_columns;
pub use rewrite_tables::{rewrite_tables, RewriteTarget, TableRef, TableRewrite, UnionRewrite};
pub use row_filter::apply_row_filter;
