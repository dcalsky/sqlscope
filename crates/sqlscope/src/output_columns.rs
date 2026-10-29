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
/// | `*` / `t.*` | Expanded from the [schema](Options::schema), CTEs and derived tables; `*` when unknown |
///
/// An unqualified `SELECT *` over several tables needs the schema of every
/// table, otherwise the result is `["*"]`; a table function makes it unknown.
/// Star modifiers (`EXCEPT` / `EXCLUDE`, `REPLACE`, `RENAME`) apply, and a
/// column shared by `JOIN ... USING` or `NATURAL JOIN` appears once. A table listed in the schema with
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
            // The parser stores `(a, LIKE src, b)` as columns [a, b] plus a
            // LIKE constraint; source positions restore the written order.
            // Without positions, LIKE columns follow the defined ones.
            let mut entries: Vec<(usize, Vec<String>)> = Vec::new();
            let mut positioned = true;
            for column in &table.columns {
                if column.name.name.is_empty() {
                    continue;
                }
                let start = column.name.span.map(|span| span.start);
                positioned &= start.is_some();
                entries.push((start.unwrap_or(usize::MAX), vec![column.name.name.clone()]));
            }
            let mut liked = false;
            for constraint in &table.constraints {
                if let TableConstraint::Like { source, .. } = constraint {
                    let start = source.name.span.map(|span| span.start);
                    positioned &= start.is_some();
                    let source = qualified_name(source);
                    let source_columns = schema::columns(schema, &source)
                        .ok_or_else(|| Error::unsupported(format!("CREATE TABLE LIKE {source} requires its schema")))?;
                    entries.push((start.unwrap_or(usize::MAX), source_columns.to_vec()));
                    liked = true;
                }
            }
            if liked && positioned {
                entries.sort_by_key(|(start, _)| *start); // stable for equal keys
            }
            let columns: Vec<String> = entries.into_iter().flat_map(|(_, columns)| columns).collect();
            if !liked && columns.is_empty() {
                return Ok(None);
            }
            Ok(Some(columns))
        }
        _ => Ok(None),
    }
}

fn query_output(query: &Expression, options: &Options) -> Result<Vec<String>> {
    let dialect = options.dialect;
    let failed = inspection_failed;

    // Inspect without the schema first: explicit names need none, and user
    // aliases such as `_col_0` must stay as written.
    let output = ast::with_large_stack(|| native_output_columns(query, Some(dialect.polyglot()))).map_err(failed)?;
    // Native inspection may call a partial CTE alias list complete. Resolve
    // top-level stars ourselves so the remaining columns are preserved.
    if let Expression::Select(select) = query {
        if select
            .expressions
            .iter()
            .any(|item| matches!(item, Expression::Star(_)))
        {
            let mut resolver = Resolver::new(options, true, false);
            let resolved = resolver.resolve_root(query);
            let mut position = 0;
            let mut names = Vec::new();
            for (item, &count) in select.expressions.iter().zip(&resolved.items) {
                let expanded = &resolved.names[position..position + count];
                position += count;
                if let Expression::Star(star) = item {
                    if expanded.iter().any(|name| name == "*") {
                        if star.table.is_none() {
                            return Ok(vec!["*".to_owned()]);
                        }
                        names.push("*".to_owned());
                        continue;
                    }
                }
                for name in expanded {
                    names.push(if name.is_empty() {
                        format!("_col{}", names.len())
                    } else {
                        name.clone()
                    });
                }
            }
            return Ok(names);
        }
    }
    // Native inspection leaves a nested field access (`t.col.field`)
    // unnamed; engines name it after the field, like `col.field`.
    let projection = naming_projection(query);
    let field_name = |index: usize| {
        projection
            .and_then(|items| items.get(index))
            .and_then(field_access_name)
    };
    if output.ordinal_complete {
        return Ok(output
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| match column {
                OutputColumn::Named { name, .. } => name.clone(),
                _ => field_name(index).unwrap_or_else(|| format!("_col{index}")),
            })
            .collect());
    }

    // Wildcards expand through the scope resolver, which shares the schema
    // lookup of the other operations and knows the columns of CTEs, derived
    // tables and star modifiers (EXCEPT, RENAME).
    // For root set operations with unresolved wildcards, let the engine
    // handle dialect-specific output-shape rules.
    if matches_by_name(query) {
        return schema_output(query, options);
    }
    let mut resolver = Resolver::new(options, true, false);
    let resolved = resolver.resolve_root(query);
    let aligned = resolved.items.len() == output.columns.len()
        && output
            .columns
            .iter()
            .zip(&resolved.items)
            .all(|(column, &count)| matches!(column, OutputColumn::Wildcard { .. }) || count == 1);
    if !aligned {
        return schema_output(query, options);
    }

    let mut names: Vec<String> = Vec::with_capacity(resolved.names.len());
    let mut position = 0;
    for (index, (column, &count)) in output.columns.iter().zip(&resolved.items).enumerate() {
        let expanded = &resolved.names[position..position + count];
        position += count;
        match column {
            OutputColumn::Named { name, .. } => names.push(name.clone()),
            OutputColumn::Unnamed { .. } => {
                names.push(field_name(index).unwrap_or_else(|| format!("_col{}", names.len())))
            }
            OutputColumn::Wildcard { qualifier, .. } => {
                if expanded.iter().any(|name| name == "*") {
                    // Some relation's columns are unknown.
                    if qualifier.is_none() {
                        return Ok(vec!["*".to_owned()]);
                    }
                    names.push("*".to_owned());
                    continue;
                }
                for name in expanded {
                    names.push(if name.is_empty() {
                        format!("_col{}", names.len())
                    } else {
                        name.clone()
                    });
                }
            }
        }
    }
    Ok(names)
}

/// The projection that names a query's columns: its own, or that of the
/// leftmost branch of a set operation.
fn naming_projection(query: &Expression) -> Option<&[Expression]> {
    match query {
        Expression::Select(select) => Some(&select.expressions),
        Expression::Union(op) => naming_projection(&op.left),
        Expression::Intersect(op) => naming_projection(&op.left),
        Expression::Except(op) => naming_projection(&op.left),
        Expression::Subquery(subquery) => naming_projection(&subquery.this),
        Expression::Paren(paren) => naming_projection(&paren.this),
        _ => None,
    }
}

fn field_access_name(item: &Expression) -> Option<String> {
    match item {
        Expression::Dot(dot) if !dot.field.name.is_empty() => Some(dot.field.name.clone()),
        _ => None,
    }
}

/// Whether a set operation in `query` matches columns by name (`UNION BY
/// NAME`, `CORRESPONDING`) rather than by position.
fn matches_by_name(query: &Expression) -> bool {
    let (left, right, by_name) = match query {
        Expression::Union(op) => (
            &op.left,
            &op.right,
            op.by_name || op.corresponding || !op.on_columns.is_empty(),
        ),
        Expression::Intersect(op) => (
            &op.left,
            &op.right,
            op.by_name || op.corresponding || !op.on_columns.is_empty(),
        ),
        Expression::Except(op) => (
            &op.left,
            &op.right,
            op.by_name || op.corresponding || !op.on_columns.is_empty(),
        ),
        Expression::Subquery(subquery) => return matches_by_name(&subquery.this),
        Expression::Paren(paren) => return matches_by_name(&paren.this),
        _ => return false,
    };
    by_name || matches_by_name(left) || matches_by_name(right)
}

/// Wildcard expansion by the engine, for projections the resolver's items do
/// not line up with.
fn schema_output(query: &Expression, options: &Options) -> Result<Vec<String>> {
    let dialect = options.dialect;
    let failed = inspection_failed;
    let Some(validation) = schema::to_validation_schema(&options.schema) else {
        return Ok(vec!["*".to_owned()]);
    };
    let mapping = mapping_schema_from_validation_schema_with_dialect(&validation, dialect.polyglot());
    let output = ast::with_large_stack(|| output_columns_with_schema(query, Some(&mapping), Some(dialect.polyglot())))
        .map_err(failed)?;
    let mut names = Vec::with_capacity(output.columns.len());
    for (index, column) in output.columns.iter().enumerate() {
        match column {
            OutputColumn::Named { name, .. } => names.push(synthetic_name(name)),
            OutputColumn::Unnamed { .. } => names.push(format!("_col{index}")),
            OutputColumn::Wildcard { qualifier: None, .. } => return Ok(vec!["*".to_owned()]),
            OutputColumn::Wildcard { .. } => names.push("*".to_owned()),
        }
    }
    Ok(names)
}

/// Classifies a failed native inspection: input the engine rejects (`UNION
/// BY NAME` over different columns, ...) is unsupported, not a bug.
fn inspection_failed(error: polyglot_sql::Error) -> Error {
    match error {
        polyglot_sql::Error::InvalidInput(_) | polyglot_sql::Error::Unsupported { .. } => {
            Error::unsupported(format!("output column inspection failed: {error}"))
        }
        _ => Error::internal(format!("output column inspection failed: {error}")),
    }
}

/// Converts the engine's `_col_{n}` to the documented `_col{n}`.
fn synthetic_name(name: &str) -> String {
    match name.strip_prefix("_col_") {
        Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => format!("_col{digits}"),
        _ => name.to_owned(),
    }
}
