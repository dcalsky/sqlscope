use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use polyglot_sql::Expression;

use crate::ast;
use crate::error::Result;
use crate::normalize::normalize;
use crate::options::Options;
use crate::resolver::Resolver;

/// The SQL clause that contains a column reference.
///
/// References inside nested queries are classified by the nested query's own
/// clause: a column in a scalar subquery's SELECT list is [`Clause::Select`]
/// even when the subquery appears in an outer WHERE.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Clause {
    Select,
    From,
    JoinOn,
    JoinUsing,
    Where,
    GroupBy,
    Having,
    Qualify,
    Window,
    OrderBy,
    SortBy,
    DistributeBy,
    ClusterBy,
    ConnectBy,
    LateralView,
    UpdateSetTarget,
    UpdateSetValue,
    MergeOn,
    MergeWhen,
}

impl Clause {
    /// The stable name: `SELECT`, `JOIN_ON`, `UPDATE_SET_VALUE`, ...
    pub fn as_str(self) -> &'static str {
        match self {
            Clause::Select => "SELECT",
            Clause::From => "FROM",
            Clause::JoinOn => "JOIN_ON",
            Clause::JoinUsing => "JOIN_USING",
            Clause::Where => "WHERE",
            Clause::GroupBy => "GROUP_BY",
            Clause::Having => "HAVING",
            Clause::Qualify => "QUALIFY",
            Clause::Window => "WINDOW",
            Clause::OrderBy => "ORDER_BY",
            Clause::SortBy => "SORT_BY",
            Clause::DistributeBy => "DISTRIBUTE_BY",
            Clause::ClusterBy => "CLUSTER_BY",
            Clause::ConnectBy => "CONNECT_BY",
            Clause::LateralView => "LATERAL_VIEW",
            Clause::UpdateSetTarget => "UPDATE_SET_TARGET",
            Clause::UpdateSetValue => "UPDATE_SET_VALUE",
            Clause::MergeOn => "MERGE_ON",
            Clause::MergeWhen => "MERGE_WHEN",
        }
    }
}

impl fmt::Display for Clause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One distinct use of a root physical-table column in a clause.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnUsage {
    /// The table as written in the query (fully qualified when the query is).
    pub table: String,
    pub column: String,
    pub clause: Clause,
}

impl Ord for ColumnUsage {
    fn cmp(&self, other: &Self) -> Ordering {
        (&self.table, &self.column, self.clause.as_str()).cmp(&(&other.table, &other.column, other.clause.as_str()))
    }
}

impl PartialOrd for ColumnUsage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Columns referenced anywhere in a statement, keyed by root physical table.
///
/// Unlike [`column_origins`](crate::column_origins), filter, join, grouping,
/// ordering and window positions count, so the result is a superset of it.
/// Accepts query-bearing statements (SELECT, set operations, CREATE VIEW,
/// CREATE TABLE AS, INSERT ... SELECT, ...) and DELETE, UPDATE and MERGE.
///
/// - CTEs and derived tables are resolved through to their root tables.
/// - A qualified column is attributed to the relation its qualifier names.
/// - An unqualified column is attributed to every source whose
///   [schema](Options::schema) declares it; a source with unknown schema is
///   always a candidate.
/// - `*` expands from the schema; otherwise it is recorded as the column `*`.
/// - A reference that cannot be resolved is attributed to every candidate
///   table rather than dropped.
///
/// Every physical table read is present, possibly with no columns. Columns are
/// sorted and distinct.
///
/// ```
/// use sqlscope::{referenced_columns, Options};
///
/// let columns = referenced_columns("SELECT id FROM orders WHERE status = 'PAID'", &Options::new())?;
/// assert_eq!(columns["orders"], ["id", "status"]);
/// # Ok::<(), sqlscope::Error>(())
/// ```
pub fn referenced_columns(sql: &str, options: &Options) -> Result<BTreeMap<String, Vec<String>>> {
    let statement = parse(sql, options)?;
    let mut resolver = Resolver::new(&options.schema, false, false);
    resolve(&statement, &mut resolver);
    Ok(std::mem::take(&mut resolver.result)
        .into_iter()
        .map(|(table, columns)| (table, columns.into_iter().collect()))
        .collect())
}

/// Every distinct `(table, column, clause)` use in a statement, sorted.
///
/// Accepts the same statements and follows the same resolution rules as
/// [`referenced_columns`]. Projection aliases and output ordinals in GROUP
/// BY / ORDER BY / HAVING / QUALIFY resolve to the projected source columns.
/// A table with no column reference has no entry.
///
/// ```
/// use sqlscope::{column_usages, Clause, ColumnUsage, Options};
///
/// let usages = column_usages("SELECT id FROM orders WHERE status = 'PAID'", &Options::new())?;
/// assert_eq!(usages[1], ColumnUsage { table: "orders".into(), column: "status".into(), clause: Clause::Where });
/// # Ok::<(), sqlscope::Error>(())
/// ```
pub fn column_usages(sql: &str, options: &Options) -> Result<Vec<ColumnUsage>> {
    let statement = parse(sql, options)?;
    let mut resolver = Resolver::new(&options.schema, false, true);
    resolve(&statement, &mut resolver);
    Ok(resolver.usages.take().unwrap_or_default().into_iter().collect())
}

fn parse(sql: &str, options: &Options) -> Result<Expression> {
    ast::parse_single(&normalize(sql, options.dialect), options.dialect)
}

fn resolve<'a>(statement: &'a Expression, resolver: &mut Resolver<'a, '_>) {
    match statement {
        Expression::Delete(delete) => resolver.resolve_delete(delete),
        Expression::Update(update) => resolver.resolve_update(update),
        Expression::Merge(merge) => resolver.resolve_merge(merge),
        _ => {
            if let Some(query) = ast::inner_query(statement) {
                resolver.resolve_root(query);
            }
        }
    }
}
