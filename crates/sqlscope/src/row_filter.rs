use std::collections::HashSet;
use std::sync::Arc;

use polyglot_sql::expressions::{Select, TableRef, Where};
use polyglot_sql::Expression;
use regex::Regex;

use crate::ast::{self, opt_name};
use crate::error::{Error, Result};
use crate::normalize::{prepare_rewrite, strip_comments};
use crate::options::{Dialect, Options};
use crate::rewrite::{
    derived_table, rewrite_statement, select_from, star, unaliased, unqualified_table_keys, AliasRef, Replacement,
};

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
/// CTE references, table functions and the `DUAL` dummy table (Oracle, the
/// MySQL family, Snowflake) are never wrapped. Reads the
/// rewrite cannot scope are rejected as unsupported: a data-modifying CTE, a
/// query CTE named like a table the predicate reads, and tables named by
/// expressions or read by functions (`IDENTIFIER(...)`, ClickHouse
/// `remote(...)`, ...).
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
    let predicate_text = predicate;
    let predicate = parse_predicate(predicate, dialect)?;

    let normalized = prepare_rewrite(sql, dialect);
    let statement = ast::parse_query(&normalized, dialect)?;
    let filter = RowFilter {
        reads: Arc::new(unqualified_table_keys(&predicate)),
        predicate: Arc::new(predicate),
    };
    match rewrite_statement(statement, dialect, |table| {
        (!is_dummy_dual(table, dialect) && scope.contains(table)).then(|| filter.clone())
    })? {
        None => Ok(sql.to_owned()),
        Some(rewritten) => {
            let probe = format!("SELECT 1 WHERE {}", strip_comments(predicate_text, dialect));
            ast::generate_checked(&rewritten, dialect, &[&normalized, &probe])
        }
    }
}

#[derive(Clone)]
struct RowFilter {
    predicate: Arc<Expression>,
    /// Unqualified tables read by subqueries in the predicate.
    reads: Arc<Vec<String>>,
}

impl Replacement for RowFilter {
    fn unqualified_reads(&self) -> Vec<String> {
        self.reads.as_ref().clone()
    }

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
    ast::guard_input(text).map_err(|error| error.context("predicate"))?;
    let text = strip_comments(text, dialect);
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
    let bare = ast::generate(&Expression::Select(Box::new(bare)), dialect)?;
    if ast::generate(&statement, dialect)? != bare {
        return Err(invalid());
    }
    // A parser that skips trailing input (ClickHouse reads `t = 1 WHERE x`
    // as `t = 1`) cannot do so inside parentheses: the text must read the
    // same as a parenthesized expression.
    let wrapped = ast::parse_single(&format!("SELECT 1 WHERE ({text})"), dialect).map_err(|_| invalid())?;
    let same = match &wrapped {
        Expression::Select(select) => match select.where_clause.as_ref().map(|w| &w.this) {
            Some(Expression::Paren(paren)) => {
                let mut unwrapped = Select::new();
                unwrapped.expressions = select.expressions.clone();
                unwrapped.where_clause = Some(Where {
                    this: paren.this.clone(),
                });
                ast::generate(&Expression::Select(Box::new(unwrapped)), dialect)? == bare
            }
            _ => false,
        },
        _ => false,
    };
    if !same {
        return Err(invalid());
    }
    Ok(condition)
}

/// `DUAL`, the one-row dummy table of Oracle, the MySQL family and
/// Snowflake. Elsewhere, and when quoted or qualified, `dual` names a table.
fn is_dummy_dual(table: &TableRef, dialect: Dialect) -> bool {
    use polyglot_sql::DialectType::{Doris, MySQL, Oracle, SingleStore, Snowflake, StarRocks, TiDB};
    matches!(
        dialect.polyglot(),
        Oracle | MySQL | SingleStore | TiDB | StarRocks | Doris | Snowflake
    ) && table.schema.is_none()
        && table.catalog.is_none()
        && !table.name.quoted
        && table.name.name.eq_ignore_ascii_case("dual")
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
