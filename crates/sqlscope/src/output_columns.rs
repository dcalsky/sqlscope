use polyglot_sql::expressions::TableConstraint;
use polyglot_sql::lineage::{output_columns as native_output_columns, output_columns_with_schema, OutputColumn};
use polyglot_sql::{mapping_schema_from_validation_schema_with_dialect, Expression};

use crate::ast::{self, qualified_name};
use crate::error::{Error, Result};
use crate::normalize::normalize;
use crate::options::Options;
use crate::resolver::Resolver;
use crate::schema;

/// The names of the columns a statement outputs, in projection order.
///
/// Accepts query-bearing statements (SELECT, set operations, CREATE VIEW,
/// CREATE TABLE AS, INSERT ... SELECT, ...) and CREATE TABLE definitions. An
/// explicit column list on CREATE VIEW, CREATE TABLE or INSERT overrides the
/// names inferred from the query. Returns `None` for statements that define
/// no columns (for example DROP TABLE).
///
/// | Projection | Name |
/// | --- | --- |
/// | `expr AS x`, `t.x`, `x` | `x` |
/// | Unaliased expression at position *i* | `_col{i}` |
/// | `*` / `t.*` | Expanded from the [schema](Options::schema); `*` when unknown |
///
/// An unqualified `SELECT *` over several tables needs the schema of every
/// table, otherwise the result is `["*"]`. A table listed in the schema with
/// no columns contributes none. `CREATE TABLE ... (LIKE t)` requires the
/// schema of `t`, and `INSERT INTO t VALUES (...)` without a column list is
/// unsupported.
///
/// ```
/// use sqlscope::{output_columns, Options};
///
/// let columns = output_columns("SELECT id, amount AS total, count(*) FROM orders", &Options::new())?;
/// assert_eq!(columns.unwrap(), ["id", "total", "_col2"]);
/// # Ok::<(), sqlscope::Error>(())
/// ```
pub fn output_columns(sql: &str, options: &Options) -> Result<Option<Vec<String>>> {
    let dialect = options.dialect;
    let statement = ast::parse_single(&normalize(sql, dialect), dialect)?;

    if let Expression::Insert(insert) = &statement {
        if !insert.columns.is_empty() {
            return Ok(Some(names(insert.columns.iter().map(|c| c.name.as_str()))));
        }
        if !insert.values.is_empty() || insert.default_values {
            return Err(Error::unsupported("INSERT without a target column list"));
        }
    }
    if let Some(columns) = declared_columns(&statement, &options.schema)? {
        return Ok(Some(columns));
    }
    let Some(query) = ast::inner_query(&statement) else {
        return Ok(None);
    };
    query_output(query, options).map(Some)
}

fn names<'n>(names: impl Iterator<Item = &'n str>) -> Vec<String> {
    names.filter(|name| !name.is_empty()).map(str::to_owned).collect()
}

/// Columns declared by DDL: an explicit view / CTAS column list, or the
/// column definitions and LIKE clauses of CREATE TABLE.
fn declared_columns(statement: &Expression, schema: &schema::Schema) -> Result<Option<Vec<String>>> {
    match statement {
        Expression::CreateView(view) if !view.columns.is_empty() => {
            Ok(Some(names(view.columns.iter().map(|c| c.name.name.as_str()))))
        }
        Expression::CreateTable(table) if table.as_select.is_some() => {
            if table.columns.is_empty() {
                Ok(None)
            } else {
                Ok(Some(names(table.columns.iter().map(|c| c.name.name.as_str()))))
            }
        }
        Expression::CreateTable(table) => {
            let mut columns = names(table.columns.iter().map(|c| c.name.name.as_str()));
            let mut liked: Vec<String> = Vec::new();
            let mut like_count = 0;
            for constraint in &table.constraints {
                if let TableConstraint::Like { source, .. } = constraint {
                    let source = qualified_name(source);
                    let source_columns = schema::columns(schema, &source)
                        .ok_or_else(|| Error::unsupported(format!("CREATE TABLE LIKE {source} requires its schema")))?;
                    liked.extend(source_columns.iter().cloned());
                    like_count += 1;
                }
            }
            if like_count == 0 {
                return Ok((!columns.is_empty()).then_some(columns));
            }
            // The parser stores `(a, LIKE src, b)` as columns [a, b] plus one
            // LIKE constraint; the LIKE columns belong after the first column.
            if columns.len() >= 2 && like_count == 1 {
                let rest = columns.split_off(1);
                columns.extend(liked);
                columns.extend(rest);
            } else {
                columns.extend(liked);
            }
            Ok(Some(columns))
        }
        _ => Ok(None),
    }
}

fn query_output(query: &Expression, options: &Options) -> Result<Vec<String>> {
    let dialect = options.dialect;
    let failed = |error: polyglot_sql::Error| Error::internal(format!("output column inspection failed: {error}"));

    // Inspect without the schema first: explicit names need none, and user
    // aliases such as `_col_0` must stay as written.
    let mut output = native_output_columns(query, Some(dialect.polyglot())).map_err(failed)?;
    let mut expanded = false;
    if !output.ordinal_complete {
        if let Some(validation) = schema::to_validation_schema(&options.schema) {
            let mapping = mapping_schema_from_validation_schema_with_dialect(&validation, dialect.polyglot());
            output = output_columns_with_schema(query, Some(&mapping), Some(dialect.polyglot())).map_err(failed)?;
            expanded = true;
        }
    }

    // polyglot treats a table with an empty column list as open (unknown);
    // here it means the table has zero columns. Fall back to the scope
    // resolver for that case.
    if !output.ordinal_complete && options.schema.values().any(Vec::is_empty) {
        let mut resolver = Resolver::new(&options.schema, true, false);
        let resolved = resolver.resolve_root(query);
        let reads_empty_table = resolver
            .result
            .keys()
            .any(|table| schema::columns(&options.schema, table).is_some_and(<[String]>::is_empty));
        if reads_empty_table {
            let unqualified_star_unresolved = output
                .columns
                .iter()
                .any(|column| matches!(column, OutputColumn::Wildcard { qualifier: None, .. }));
            let mut names = Vec::with_capacity(resolved.names.len());
            for (index, name) in resolved.names.iter().enumerate() {
                if name == "*" && unqualified_star_unresolved {
                    return Ok(vec!["*".to_owned()]);
                }
                names.push(if name.is_empty() {
                    format!("_col{index}")
                } else {
                    name.clone()
                });
            }
            return Ok(names);
        }
    }

    let mut names = Vec::with_capacity(output.columns.len());
    for (index, column) in output.columns.iter().enumerate() {
        match column {
            OutputColumn::Named { name, .. } => names.push(if expanded { synthetic_name(name) } else { name.clone() }),
            OutputColumn::Unnamed { .. } => names.push(format!("_col{index}")),
            OutputColumn::Wildcard { qualifier: None, .. } => return Ok(vec!["*".to_owned()]),
            OutputColumn::Wildcard { .. } => names.push("*".to_owned()),
        }
    }
    Ok(names)
}

/// Converts the engine's `_col_{n}` to the documented `_col{n}`.
fn synthetic_name(name: &str) -> String {
    match name.strip_prefix("_col_") {
        Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => format!("_col{digits}"),
        _ => name.to_owned(),
    }
}
