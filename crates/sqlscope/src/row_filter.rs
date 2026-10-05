use std::collections::HashSet;
use std::sync::Arc;

use polyglot_sql::expressions::{Select, TableRef, Where};
use polyglot_sql::Expression;
use regex::Regex;

use crate::ast::{self, opt_name};
use crate::error::{Error, Result};
use crate::normalize::normalize;
use crate::options::{Dialect, Options};
use crate::rewrite::{derived_table, rewrite_statement, select_from, star, unaliased, AliasRef, Replacement};

/// Wraps every in-scope physical table of a query in a derived table filtered
/// by `predicate`.
///
/// ```text
/// SELECT * FROM a   ->   SELECT * FROM (SELECT * FROM a WHERE <predicate>) AS a
/// ```
///
/// Filtering each table before it is joined or combined keeps the filter
/// correct across outer joins, CTEs, subqueries and set operations. The input
/// must be a single `SELECT` or set operation; anything else is rejected with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported).
///
/// `predicate` is parsed as one boolean expression of the selected dialect and
/// inserted into the AST. It is never spliced into SQL text, but its own
/// values must already be bound or escaped by the caller.
///
/// By default every physical table is filtered. [`Options::table_names`],
/// [`Options::table_patterns`] and [`Options::default_db`] restrict the scope.
/// CTE references, table functions and `DUAL` are never wrapped.
///
/// Rewritten output is regenerated from the AST (normalized formatting, no
/// comments) and verified to parse. When no table is in scope the input is
/// returned unchanged.
///
/// ```
/// use sqlscope::{apply_row_filter, Dialect, Options};
///
/// let sql = apply_row_filter(
///     "SELECT id FROM orders",
///     "tenant_id = 7",
///     &Options::new().dialect(Dialect::POSTGRES),
/// )?;
/// assert_eq!(sql, "SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders");
/// # Ok::<(), sqlscope::Error>(())
/// ```
pub fn apply_row_filter(sql: &str, predicate: &str, options: &Options) -> Result<String> {
    let dialect = options.dialect;
    let scope = TableScope::compile(options)?;
    let predicate = parse_predicate(predicate, dialect)?;

    let normalized = normalize(sql, dialect);
    let statement = ast::parse_query(&normalized, dialect)?;
    let filter = RowFilter {
        predicate: Arc::new(predicate),
    };
    match rewrite_statement(statement, |table| scope.contains(table).then(|| filter.clone()))? {
        None => Ok(sql.to_owned()),
        Some(rewritten) => ast::generate_checked(&ast::strip_comments(rewritten)?, dialect),
    }
}

#[derive(Clone)]
struct RowFilter {
    predicate: Arc<Expression>,
}

impl Replacement for RowFilter {
    fn build(&self, original: TableRef, alias: &AliasRef) -> Result<Expression> {
        let column_aliases = original.column_aliases.clone();
        let mut select = select_from(vec![star()], unaliased(original));
        select.where_clause = Some(Where {
            this: (*self.predicate).clone(),
        });
        Ok(derived_table(
            Expression::Select(Box::new(select)),
            alias,
            column_aliases,
        ))
    }
}

/// Parses `text` as exactly one boolean expression.
fn parse_predicate(text: &str, dialect: Dialect) -> Result<Expression> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::invalid("predicate must not be empty"));
    }
    ast::guard_input(text).map_err(|error| error.context("predicate"))?;
    let invalid = || Error::invalid(format!("predicate {text:?} is not a single boolean expression"));

    let probe = format!("SELECT 1 WHERE {text}");
    let statement = ast::parse_single(&probe, dialect).map_err(|error| match error.kind() {
        crate::ErrorKind::Parse => Error::parse(format!("invalid predicate {text:?}: {}", error.message())),
        crate::ErrorKind::Unsupported if error.message().contains("E_GUARD_") => error.context("predicate"),
        _ => invalid(),
    })?;
    let Expression::Select(select) = &statement else {
        return Err(invalid());
    };
    let Some(condition) = select.where_clause.as_ref().map(|w| w.this.clone()) else {
        return Err(invalid());
    };

    // Anything the text added beyond the WHERE condition (ORDER BY, LIMIT,
    // GROUP BY, ...) would be silently lost; regenerating the probe without
    // it exposes such trailing clauses.
    let mut bare = Select::new();
    bare.expressions = select.expressions.clone();
    bare.where_clause = Some(Where {
        this: condition.clone(),
    });
    if ast::generate(&statement, dialect)? != ast::generate(&Expression::Select(Box::new(bare)), dialect)? {
        return Err(invalid());
    }
    Ok(condition)
}

/// The set of tables `apply_row_filter` wraps.
struct TableScope {
    /// When false, every physical table is in scope.
    restricted: bool,
    catalog_keys: HashSet<(String, String, String)>,
    schema_keys: HashSet<(String, String)>,
    names: HashSet<String>,
    patterns: Vec<Regex>,
    default_db: String,
}

impl TableScope {
    fn compile(options: &Options) -> Result<Self> {
        let mut scope = TableScope {
            restricted: options.table_names.is_some() || !options.table_patterns.is_empty(),
            catalog_keys: HashSet::new(),
            schema_keys: HashSet::new(),
            names: HashSet::new(),
            patterns: Vec::new(),
            default_db: options.default_db.as_deref().unwrap_or_default().trim().to_lowercase(),
        };
        for name in options.table_names.iter().flatten() {
            let parts: Vec<String> = name.split('.').map(|part| part.trim().to_lowercase()).collect();
            if parts.iter().any(String::is_empty) {
                return Err(Error::invalid(format!("invalid table name {name:?}")));
            }
            match <[String; 3]>::try_from(parts) {
                Ok([catalog, schema, table]) => {
                    scope.catalog_keys.insert((catalog, schema, table));
                }
                Err(parts) => match <[String; 2]>::try_from(parts) {
                    Ok([schema, table]) => {
                        scope.schema_keys.insert((schema, table));
                    }
                    Err(parts) => match <[String; 1]>::try_from(parts) {
                        Ok([table]) => {
                            scope.names.insert(table);
                        }
                        Err(_) => return Err(Error::invalid(format!("table name {name:?} has more than three parts"))),
                    },
                },
            }
        }
        for pattern in &options.table_patterns {
            if pattern.trim().is_empty() {
                return Err(Error::invalid("table pattern must not be empty"));
            }
            let regex = Regex::new(pattern)
                .map_err(|error| Error::invalid(format!("invalid table pattern {pattern:?}: {error}")))?;
            scope.patterns.push(regex);
        }
        if scope.restricted
            && scope.catalog_keys.is_empty()
            && scope.schema_keys.is_empty()
            && scope.names.is_empty()
            && scope.patterns.is_empty()
        {
            return Err(Error::invalid("table scope was provided but names no tables"));
        }
        Ok(scope)
    }

    fn contains(&self, table: &TableRef) -> bool {
        let name = table.name.name.to_lowercase();
        if name.is_empty() {
            return false;
        }
        let schema = opt_name(&table.schema).to_lowercase();
        let catalog = opt_name(&table.catalog).to_lowercase();
        if schema.is_empty() && name == "dual" {
            return false;
        }
        if !self.restricted {
            return true;
        }
        if !catalog.is_empty()
            && !schema.is_empty()
            && self
                .catalog_keys
                .contains(&(catalog.clone(), schema.clone(), name.clone()))
        {
            return true;
        }
        let resolved_schema = if schema.is_empty() { &self.default_db } else { &schema };
        if !resolved_schema.is_empty() && self.schema_keys.contains(&(resolved_schema.clone(), name.clone())) {
            return true;
        }
        if self.names.contains(&name) {
            return true;
        }
        if self.patterns.is_empty() {
            return false;
        }
        let written = ast::qualified_name(table);
        self.patterns
            .iter()
            .any(|pattern| pattern.is_match(&table.name.name) || pattern.is_match(&written))
    }
}
