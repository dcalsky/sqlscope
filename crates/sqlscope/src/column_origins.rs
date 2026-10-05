use std::collections::{BTreeMap, BTreeSet};

use polyglot_sql::openlineage::{openlineage_column_lineage, OpenLineageDatasetId, OpenLineageOptions};
use polyglot_sql::{analyze_query, AnalyzeQueryOptions, Expression};

use crate::ast::{self, table_names_match};
use crate::error::{Error, Result};
use crate::normalize::normalize;
use crate::options::Options;
use crate::resolver::Resolver;
use crate::schema;

const NAMESPACE: &str = "sqlscope";
const PRODUCER: &str = "https://github.com/dcalsky/sqlscope";

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
        Some(query) => query_origins(query, options)?,
        None => dml_origins(&statement, options),
    };
    Ok(std::mem::take(&mut origins)
        .into_iter()
        .map(|(table, columns)| (table, columns.into_iter().collect()))
        .collect())
}

/// UPDATE and MERGE: polyglot's lineage engine does not accept them, so the
/// assigned values are resolved structurally.
fn dml_origins(statement: &Expression, options: &Options) -> BTreeMap<String, BTreeSet<String>> {
    let mut resolver = Resolver::new(&options.schema, true, false);
    match statement {
        Expression::Update(update) => resolver.update_value_flow(update),
        Expression::Merge(merge) => resolver.merge_value_flow(merge),
        _ => return BTreeMap::new(),
    }
    std::mem::take(&mut resolver.result)
}

fn query_origins(query: &Expression, options: &Options) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let dialect = options.dialect;
    let query_sql = ast::generate(query, dialect)?;

    // Root physical tables, seen through CTEs, subqueries and set operations.
    let analysis = analyze_query(
        &query_sql,
        AnalyzeQueryOptions {
            dialect: dialect.polyglot(),
            ..AnalyzeQueryOptions::default()
        },
    )
    .map_err(|error| Error::internal(format!("query analysis failed: {error}")))?;
    let sources: Vec<String> = analysis
        .base_tables
        .into_iter()
        .map(|table| table.name)
        .filter(|name| !name.is_empty())
        .collect();
    let mut origins: BTreeMap<String, BTreeSet<String>> =
        sources.iter().map(|source| (source.clone(), BTreeSet::new())).collect();

    let lineage = openlineage_column_lineage(
        &query_sql,
        &OpenLineageOptions {
            dialect: dialect.polyglot(),
            producer: PRODUCER.to_owned(),
            dataset_namespace: Some(NAMESPACE.to_owned()),
            output_dataset: Some(OpenLineageDatasetId::new(NAMESPACE, "result")),
            schema: schema::to_validation_schema(&schema::restrict_to(&options.schema, &sources)),
            ..OpenLineageOptions::default()
        },
    )
    .map_err(|error| Error::internal(format!("column lineage failed: {error}")))?;

    // A source can be both DIRECT and FILTER for one output (the right branch
    // of `x EXCEPT x`); any DIRECT transformation makes it a value source.
    let mut sourceless = false;
    for field in lineage.facet.fields.values() {
        if field.input_fields.is_empty() {
            sourceless = true;
        }
        for input in &field.input_fields {
            let direct = input
                .transformations
                .iter()
                .any(|transformation| transformation.type_.trim().eq_ignore_ascii_case("DIRECT"));
            if input.name.is_empty() || input.field.is_empty() || !direct {
                continue;
            }
            if let Some(table) = scope_table(&origins, &input.name) {
                origins
                    .get_mut(&table)
                    .expect("known table")
                    .insert(input.field.clone());
            }
        }
    }

    // Outputs without any source column (count(*), literals, scalar
    // subqueries, aggregates the engine leaves unresolved) only expose their
    // output name, which is not a source column. Recover the real columns
    // that flow into the result structurally instead.
    if sourceless {
        let mut resolver = Resolver::new(&options.schema, true, false);
        let out = resolver.resolve_root(query);
        for name in &out.names {
            for (table, column) in out.refs(name).into_iter().flatten() {
                if table.is_empty() || column.is_empty() || column == "*" {
                    continue;
                }
                if let Some(table) = scope_table(&origins, table) {
                    origins.get_mut(&table).expect("known table").insert(column.clone());
                }
            }
        }
    }
    Ok(origins)
}

/// Maps a lineage dataset name onto the unique seeded source table it names.
fn scope_table(origins: &BTreeMap<String, BTreeSet<String>>, name: &str) -> Option<String> {
    if origins.contains_key(name) {
        return Some(name.to_owned());
    }
    let mut candidates = origins.keys().filter(|table| table_names_match(name, table));
    match (candidates.next(), candidates.next()) {
        (Some(table), None) => Some(table.clone()),
        _ => None,
    }
}
