use std::collections::{BTreeMap, BTreeSet};

use polyglot_sql::Expression;

use crate::ast;
use crate::error::Result;
use crate::normalize::normalize;
use crate::options::Options;
use crate::resolver::Resolver;

/// The source columns whose values flow into a statement's result, keyed by
/// root physical table.
///
/// Only columns that reach the result count: columns used only by WHERE,
/// JOIN ... ON, GROUP BY, HAVING or ORDER BY are excluded, as are the right
/// branches of INTERSECT and EXCEPT (they filter rows but contribute no
/// value). Window PARTITION BY / ORDER BY feed the window value and count.
///
/// Accepts every statement that contains a query (SELECT, set operations,
/// CREATE VIEW, CREATE TABLE AS, INSERT ... SELECT, ...). For UPDATE and MERGE
/// the assigned values count. Other statements yield an empty map.
///
/// The [schema](Options::schema) expands wildcards and attributes unqualified
/// columns; entries for tables the query does not read are ignored. Every
/// physical table read is present, possibly with no columns.
///
/// ```
/// use sqlscope::{column_origins, Options};
///
/// let origins = column_origins(
///     "SELECT o.id, p.amount FROM orders o JOIN payments p ON o.id = p.order_id",
///     &Options::new(),
/// )?;
/// assert_eq!(origins["orders"], ["id"]);
/// assert_eq!(origins["payments"], ["amount"]);
/// # Ok::<(), sqlscope::Error>(())
/// ```
pub fn column_origins(sql: &str, options: &Options) -> Result<BTreeMap<String, Vec<String>>> {
    let dialect = options.dialect;
    let statement = ast::parse_single(&normalize(sql, dialect), dialect)?;

    let mut origins: BTreeMap<String, BTreeSet<String>> = match ast::inner_query(&statement) {
        Some(query) => query_origins(query, options),
        None => dml_origins(&statement, options),
    };
    Ok(std::mem::take(&mut origins)
        .into_iter()
        .map(|(table, columns)| (table, columns.into_iter().collect()))
        .collect())
}

/// UPDATE and MERGE: the values assigned by SET and MERGE actions.
fn dml_origins(statement: &Expression, options: &Options) -> BTreeMap<String, BTreeSet<String>> {
    let mut resolver = Resolver::new(&options.schema, true, false);
    match statement {
        Expression::Update(update) => resolver.update_value_flow(update),
        Expression::Merge(merge) => resolver.merge_value_flow(merge),
        _ => return BTreeMap::new(),
    }
    std::mem::take(&mut resolver.result)
}

fn query_origins(query: &Expression, options: &Options) -> BTreeMap<String, BTreeSet<String>> {
    // Every physical table the query reads, including filter-only ones.
    let mut tables = Resolver::new(&options.schema, false, false);
    tables.resolve_root(query);
    let mut origins: BTreeMap<String, BTreeSet<String>> = std::mem::take(&mut tables.result)
        .into_keys()
        .map(|table| (table, BTreeSet::new()))
        .collect();

    // The refs behind each output position are exactly the values that
    // reach the result.
    let mut flow = Resolver::new(&options.schema, true, false);
    let out = flow.resolve_root(query);
    for (table, column) in out.positions.iter().flatten() {
        if column.is_empty() || column == "*" {
            continue;
        }
        if let Some(columns) = origins.get_mut(table) {
            columns.insert(column.clone());
        }
    }
    origins
}
