use std::collections::{HashMap, HashSet};

use polyglot_sql::expressions::{TableRef as AstTable, Union};
use polyglot_sql::Expression;

use crate::ast::{self, ident, opt_name};
use crate::error::{Error, Result};
use crate::normalize::prepare_rewrite;
use crate::options::Options;
use crate::rewrite::{column, derived_table, rewrite_statement, select_from, star, AliasRef, Replacement};

/// A table name, optionally schema- and catalog-qualified.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub table: String,
}

impl TableRef {
    /// An unqualified table.
    pub fn new(table: impl Into<String>) -> Self {
        Self {
            catalog: None,
            schema: None,
            table: table.into(),
        }
    }

    /// Sets the schema qualifier.
    pub fn with_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    /// Sets the catalog qualifier (which requires a schema).
    pub fn with_catalog(mut self, catalog: impl Into<String>) -> Self {
        self.catalog = Some(catalog.into());
        self
    }

    fn validate(&self) -> Result<(), String> {
        let present = |part: &Option<String>| part.as_deref().is_some_and(|p| !p.trim().is_empty());
        if self.table.trim().is_empty() {
            return Err("target table must not be empty".into());
        }
        if present(&self.catalog) && !present(&self.schema) {
            return Err("target catalog requires a schema".into());
        }
        Ok(())
    }

    fn to_ast(&self) -> AstTable {
        let part = |name: &Option<String>| {
            name.as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(ident)
        };
        let mut table = AstTable::new("");
        table.name = ident(&self.table);
        table.schema = part(&self.schema);
        table.catalog = part(&self.catalog);
        table
    }
}

/// A derived table backed by `UNION DISTINCT` over several tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnionRewrite {
    /// Alias for references that have none of their own. An explicit alias
    /// on the matched reference always wins.
    pub table_alias: String,
    /// The columns projected from every branch.
    pub columns: Vec<String>,
    /// The tables combined with `UNION DISTINCT`.
    pub branches: Vec<TableRef>,
}

/// What a matched table reference becomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewriteTarget {
    /// `(SELECT * FROM <table>)`.
    Inline(TableRef),
    /// `(SELECT <columns> FROM <b1> UNION DISTINCT SELECT <columns> FROM <b2> ...)`.
    Union(UnionRewrite),
}

/// One entry of a table rewrite plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRewrite {
    /// The `schema.table` to match, case-insensitively. References with a
    /// catalog match only when the catalog is listed in
    /// [`Options::strip_catalogs`].
    pub match_key: String,
    pub target: RewriteTarget,
}

impl TableRewrite {
    /// Replaces `match_key` with `(SELECT * FROM target)`.
    pub fn inline(match_key: impl Into<String>, target: TableRef) -> Self {
        Self {
            match_key: match_key.into(),
            target: RewriteTarget::Inline(target),
        }
    }

    /// Replaces `match_key` with a `UNION DISTINCT` derived table.
    pub fn union(match_key: impl Into<String>, union: UnionRewrite) -> Self {
        Self {
            match_key: match_key.into(),
            target: RewriteTarget::Union(union),
        }
    }
}

/// Replaces physical table references in a query according to a plan.
///
/// Every reference whose `schema.table` matches a [`TableRewrite`] becomes a
/// derived table that keeps the reference's alias, or, when it had none, its
/// bare table name (the union's [`table_alias`](UnionRewrite::table_alias)),
/// disambiguated if two distinct tables would collide. Qualified column and
/// star references (`schema.table.col`, `catalog.schema.table.*`) are
/// rebound onto the derived alias; bare `table.col` references are rebound
/// only where the name still denotes the replaced table. CTE references are
/// never replaced.
///
/// The rewrite is structural: identifiers are inserted as AST nodes and
/// quoted by the generator as the dialect requires, so hostile names cannot
/// alter the query. Output is regenerated (normalized, comments dropped) and
/// verified to parse; when nothing matches the input is returned unchanged.
///
/// ```
/// use sqlscope::{rewrite_tables, Options, TableRef, TableRewrite};
///
/// let sql = rewrite_tables(
///     "SELECT col1 FROM sales.orders",
///     &[TableRewrite::inline("sales.orders", TableRef::new("orders_v2").with_schema("curated"))],
///     &Options::new(),
/// )?;
/// assert_eq!(sql, "SELECT col1 FROM (SELECT * FROM curated.orders_v2) AS orders");
/// # Ok::<(), sqlscope::Error>(())
/// ```
pub fn rewrite_tables(sql: &str, rewrites: &[TableRewrite], options: &Options) -> Result<String> {
    if rewrites.is_empty() {
        return Ok(sql.to_owned());
    }
    let dialect = options.dialect;
    let plan = compile_plan(rewrites)?;
    let transparent: HashSet<String> = options
        .strip_catalogs
        .iter()
        .map(|catalog| catalog.trim().to_lowercase())
        .filter(|catalog| !catalog.is_empty())
        .collect();

    let normalized = prepare_rewrite(sql, dialect);
    let statement = ast::parse_query(&normalized, dialect)?;
    let rewritten = rewrite_statement(statement, |table| {
        let key = match_key(table, &transparent)?;
        plan.get(&key).cloned()
    })?;
    match rewritten {
        None => Ok(sql.to_owned()),
        Some(rewritten) => ast::generate_checked(&rewritten, dialect),
    }
}

fn compile_plan(rewrites: &[TableRewrite]) -> Result<HashMap<String, RewriteTarget>> {
    let mut plan = HashMap::with_capacity(rewrites.len());
    for rewrite in rewrites {
        let raw = &rewrite.match_key;
        let key = raw.trim().to_lowercase();
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() != 2 || parts.iter().any(|part| part.trim().is_empty()) {
            return Err(Error::invalid(format!(
                "table rewrite match key {raw:?} must be schema.table"
            )));
        }
        let invalid = |message: String| Error::invalid(format!("table rewrite {raw:?}: {message}"));
        match &rewrite.target {
            RewriteTarget::Inline(target) => target.validate().map_err(invalid)?,
            RewriteTarget::Union(union) => {
                if union.table_alias.trim().is_empty() {
                    return Err(invalid("union rewrite requires a table alias".into()));
                }
                if union.columns.is_empty() {
                    return Err(invalid("union rewrite requires at least one column".into()));
                }
                if let Some(index) = union.columns.iter().position(|c| c.trim().is_empty()) {
                    return Err(invalid(format!("union column {index} must not be empty")));
                }
                if union.branches.is_empty() {
                    return Err(invalid("union rewrite requires at least one branch".into()));
                }
                for (index, branch) in union.branches.iter().enumerate() {
                    branch
                        .validate()
                        .map_err(|message| invalid(format!("union branch {index}: {message}")))?;
                }
            }
        }
        if plan.insert(key, rewrite.target.clone()).is_some() {
            return Err(Error::invalid(format!("duplicate table rewrite match key {raw:?}")));
        }
    }
    Ok(plan)
}

/// The lower-case `schema.table` key of a reference after dropping a
/// transparent catalog, or `None` when it cannot match a rewrite.
fn match_key(table: &AstTable, transparent: &HashSet<String>) -> Option<String> {
    let name = table.name.name.to_lowercase();
    let schema = opt_name(&table.schema).to_lowercase();
    let catalog = opt_name(&table.catalog).to_lowercase();
    if name.is_empty() || schema.is_empty() || !(catalog.is_empty() || transparent.contains(&catalog)) {
        return None;
    }
    Some(format!("{schema}.{name}"))
}

impl Replacement for RewriteTarget {
    fn default_alias(&self, table: &AstTable) -> AliasRef {
        match self {
            RewriteTarget::Union(union) => AliasRef {
                quoted: ast::needs_quote(&union.table_alias),
                name: union.table_alias.clone(),
            },
            RewriteTarget::Inline(_) => AliasRef::from_ident(&table.name),
        }
    }

    fn build(&self, original: AstTable, alias: &AliasRef) -> Result<Expression> {
        let sample = original.table_sample;
        let with_sample = |target: &TableRef| {
            let mut table = target.to_ast();
            table.table_sample = sample.clone();
            table
        };
        let query = match self {
            RewriteTarget::Inline(target) => {
                Expression::Select(Box::new(select_from(vec![star()], with_sample(target))))
            }
            RewriteTarget::Union(union) => {
                let branch = |target: &TableRef| {
                    let projection = union.columns.iter().map(|name| column(name)).collect();
                    Expression::Select(Box::new(select_from(projection, with_sample(target))))
                };
                let mut branches = union.branches.iter();
                let first = branch(branches.next().expect("validated non-empty branches"));
                branches.fold(first, |left, target| union_distinct(left, branch(target)))
            }
        };
        Ok(derived_table(query, alias, original.column_aliases))
    }
}

fn union_distinct(left: Expression, right: Expression) -> Expression {
    Expression::Union(Box::new(Union {
        left,
        right,
        all: false,
        distinct: true,
        with: None,
        order_by: None,
        limit: None,
        offset: None,
        distribute_by: None,
        sort_by: None,
        cluster_by: None,
        by_name: false,
        side: None,
        kind: None,
        corresponding: false,
        strict: false,
        on_columns: Vec::new(),
    }))
}
