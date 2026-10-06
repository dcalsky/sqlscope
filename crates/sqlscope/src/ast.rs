//! Shared helpers over the polyglot typed AST.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::mem::{discriminant, Discriminant};

use polyglot_sql::expressions::{Column, DotAccess, Identifier, TableRef, With};
use polyglot_sql::{traversal::transform_all, ComplexityGuardOptions, Expression, ExpressionWalk, ParseOptions};

use crate::error::{Error, Result};
use crate::options::Dialect;

/// Upper bound on any SQL text accepted or produced, independent of the
/// parser's own complexity guards.
pub(crate) const MAX_INPUT_BYTES: usize = 1 << 20;

pub(crate) fn guard_input(sql: &str) -> Result<()> {
    if sql.len() > MAX_INPUT_BYTES {
        return Err(Error::unsupported(format!(
            "input too large ({} bytes, limit {MAX_INPUT_BYTES})",
            sql.len()
        )));
    }
    Ok(())
}

/// Maps a polyglot parse failure to [`Error`]. Complexity-guard rejections
/// (`E_GUARD_*`) are bounded-input failures and classify as unsupported.
fn classify_parse_error(error: polyglot_sql::Error) -> Error {
    let message = error.to_string();
    if message.contains("E_GUARD_") {
        Error::unsupported(message)
    } else {
        Error::parse(message)
    }
}

/// Parser recursion limit. Set explicitly so every build target (native and
/// WebAssembly, whose polyglot defaults differ) accepts the same inputs.
pub(crate) const MAX_PARSER_DEPTH: usize = 256;

fn parse_options() -> ParseOptions {
    ParseOptions {
        complexity_guard: Some(ComplexityGuardOptions {
            max_parser_depth: Some(MAX_PARSER_DEPTH),
            ..ComplexityGuardOptions::default()
        }),
    }
}

/// Parses `sql` into its statements.
pub(crate) fn parse(sql: &str, dialect: Dialect) -> Result<Vec<Expression>> {
    guard_input(sql)?;
    let statements =
        polyglot_sql::parse_with_options(sql, dialect.polyglot(), &parse_options()).map_err(classify_parse_error)?;
    if dialect != Dialect::BIGQUERY {
        return Ok(statements);
    }
    statements.into_iter().map(split_bigquery_paths).collect()
}

/// BigQuery reads a quoted table name containing dots as a path:
/// `` `proj.ds.orders` `` is `proj`.`ds`.`orders`. The parser keeps it as one
/// identifier, which would hide the dataset and project from name matching,
/// so every table reference, and every column or star qualifier written the
/// same way, is split into its parts.
fn split_bigquery_paths(statement: Expression) -> Result<Expression> {
    with_large_stack(|| {
        transform_all(statement, &|mut node| {
            match &mut node {
                Expression::Table(table) => split_bigquery_path(table),
                Expression::Update(update) => {
                    split_bigquery_path(&mut update.table);
                    update.extra_tables.iter_mut().for_each(split_bigquery_path);
                }
                Expression::Delete(delete) => {
                    split_bigquery_path(&mut delete.table);
                    delete.using.iter_mut().for_each(split_bigquery_path);
                    delete.tables.iter_mut().for_each(split_bigquery_path);
                }
                Expression::Insert(insert) => split_bigquery_path(&mut insert.table),
                Expression::Column(column) => {
                    if let Some(chain) = column.table.as_ref().and_then(dotted_parts) {
                        return Ok(dot_chain(chain, column.name.clone()));
                    }
                }
                Expression::Star(star) => {
                    // A star qualifier is one identifier whose unquoted dots
                    // separate the parts.
                    if let Some(table) = star.table.as_mut().filter(|t| t.quoted && t.name.contains('.')) {
                        table.quoted = false;
                    }
                }
                _ => {}
            }
            Ok(node)
        })
    })
    .map_err(|error| Error::internal(format!("AST transform failed: {error}")))
}

/// The parts of a quoted, dotted identifier (`` `proj.ds.orders` ``).
fn dotted_parts(identifier: &Identifier) -> Option<Vec<Identifier>> {
    if !identifier.quoted || !identifier.name.contains('.') {
        return None;
    }
    let parts: Vec<Identifier> = identifier.name.split('.').map(Identifier::quoted).collect();
    (parts.len() <= 3 && parts.iter().all(|part| !part.name.is_empty())).then_some(parts)
}

/// `a.b.c` + `field` as a column reference followed by field accesses, the
/// shape the parser gives an unquoted `a.b.c.field`.
fn dot_chain(mut qualifier: Vec<Identifier>, field: Identifier) -> Expression {
    let rest = qualifier.split_off(2.min(qualifier.len()));
    let mut parts = qualifier.into_iter();
    let first = parts.next().expect("a dotted name has two parts");
    let second = parts.next().expect("a dotted name has two parts");
    let mut expression = Expression::Column(Box::new(Column {
        name: second,
        table: Some(first),
        join_mark: false,
        trailing_comments: Vec::new(),
        span: None,
        inferred_type: None,
    }));
    for part in rest.into_iter().chain([field]) {
        expression = Expression::Dot(Box::new(DotAccess {
            this: expression,
            field: part,
            inferred_type: None,
        }));
    }
    expression
}

fn split_bigquery_path(table: &mut TableRef) {
    let written = [table.catalog.as_ref(), table.schema.as_ref(), Some(&table.name)];
    if !written
        .iter()
        .flatten()
        .any(|identifier| dotted_parts(identifier).is_some())
    {
        return;
    }
    let mut parts: Vec<Identifier> = Vec::with_capacity(3);
    for identifier in written.into_iter().flatten() {
        match dotted_parts(identifier) {
            Some(split) => parts.extend(split),
            None if !identifier.name.is_empty() => parts.push(identifier.clone()),
            None => {}
        }
    }
    if parts.len() > 3 {
        return; // not a project.dataset.table path; leave it as written
    }
    table.name = parts.pop().expect("a dotted name has two parts");
    table.schema = parts.pop();
    table.catalog = parts.pop();
}

/// Parses `sql`, which must contain exactly one statement.
pub(crate) fn parse_single(sql: &str, dialect: Dialect) -> Result<Expression> {
    let mut statements = parse(sql, dialect)?;
    if statements.len() != 1 {
        return Err(Error::unsupported(format!(
            "expected exactly one statement, got {}",
            statements.len()
        )));
    }
    Ok(statements.remove(0))
}

/// Parses `sql`, which must be a single SELECT or set operation.
pub(crate) fn parse_query(sql: &str, dialect: Dialect) -> Result<Expression> {
    let statement = parse_single(sql, dialect)?;
    if !is_query(&statement) {
        return Err(Error::unsupported(
            "only a SELECT or set operation (UNION / INTERSECT / EXCEPT) is supported",
        ));
    }
    Ok(statement)
}

/// Renders `expression` as SQL.
pub(crate) fn generate(expression: &Expression, dialect: Dialect) -> Result<String> {
    // The generator recurses without growing the stack itself.
    with_large_stack(|| polyglot_sql::generate(expression, dialect.polyglot()))
        .map_err(|error| Error::internal(format!("SQL generation failed: {error}")))
}

/// Renders a rewritten statement and proves that the output parses again, so
/// a rewrite either yields valid SQL or fails closed.
pub(crate) fn generate_checked(expression: &Expression, dialect: Dialect) -> Result<String> {
    let sql = generate(expression, dialect)?;
    guard_input(&sql).map_err(|error| error.context("generated SQL"))?;
    if let Err(error) = polyglot_sql::parse_with_options(&sql, dialect.polyglot(), &parse_options()) {
        return Err(Error::internal(format!(
            "rewritten SQL does not parse: {error}; SQL: {sql}"
        )));
    }
    Ok(sql)
}

pub(crate) fn is_query(expression: &Expression) -> bool {
    matches!(
        expression,
        Expression::Select(_) | Expression::Union(_) | Expression::Intersect(_) | Expression::Except(_)
    )
}

/// The WITH clause attached to a query node.
pub(crate) fn query_with(expression: &Expression) -> Option<&With> {
    match expression {
        Expression::Select(select) => select.with.as_ref(),
        Expression::Union(op) => op.with.as_ref(),
        Expression::Intersect(op) => op.with.as_ref(),
        Expression::Except(op) => op.with.as_ref(),
        _ => None,
    }
}

pub(crate) fn query_with_mut(expression: &mut Expression) -> Option<&mut Option<With>> {
    match expression {
        Expression::Select(select) => Some(&mut select.with),
        Expression::Union(op) => Some(&mut op.with),
        Expression::Intersect(op) => Some(&mut op.with),
        Expression::Except(op) => Some(&mut op.with),
        _ => None,
    }
}

/// Finds the query a statement wraps: the statement itself for a SELECT or set
/// operation, or the query of CREATE VIEW / CREATE TABLE AS / INSERT ...
/// SELECT / EXPLAIN / CACHE TABLE and similar wrappers. `None` means the
/// statement contains no query (for example CREATE TABLE (...) or DROP).
pub(crate) fn inner_query(statement: &Expression) -> Option<&Expression> {
    if is_query(statement) {
        return Some(statement);
    }
    match statement {
        Expression::Subquery(subquery) if is_query(&subquery.this) => Some(&subquery.this),
        Expression::Paren(paren) => inner_query(&paren.this),
        // DML statements are handled structurally; their nested queries
        // (WHERE subqueries, MERGE sources) are not "the" query.
        Expression::Update(_) | Expression::Delete(_) | Expression::Merge(_) => None,
        _ => statement.children().into_iter().find_map(|child| {
            if is_query(child) {
                Some(child)
            } else if let Expression::Subquery(subquery) = child {
                is_query(&subquery.this).then_some(&subquery.this)
            } else {
                None
            }
        }),
    }
}

/// Runs `f`, first growing the stack when little of it is left. Wraps every
/// recursive AST walk so deep (but parser-accepted) input cannot overflow
/// native stacks. Without the `stacker` feature (WebAssembly) the module's
/// stack is sized for the parser's depth limit instead.
#[inline]
pub(crate) fn with_stack<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(feature = "stacker")]
    {
        stacker::maybe_grow(128 * 1024, 4 * 1024 * 1024, f)
    }
    #[cfg(not(feature = "stacker"))]
    {
        f()
    }
}

/// Runs `f` on a fresh, large stack segment (native builds) for library
/// calls that recurse deeply without growing the stack themselves.
pub(crate) fn with_large_stack<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(feature = "stacker")]
    {
        stacker::maybe_grow(8 * 1024 * 1024, 64 * 1024 * 1024, f)
    }
    #[cfg(not(feature = "stacker"))]
    {
        f()
    }
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Whether a caller-supplied identifier must be quoted to keep its exact
/// value: anything other than a simple lower-case identifier. Mixed or upper
/// case is quoted because folding dialects would otherwise change it.
pub(crate) fn needs_quote(name: &str) -> bool {
    let mut chars = name.chars();
    let simple = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    !simple || name.chars().any(|c| c.is_ascii_uppercase())
}

/// An identifier node for a caller-supplied name.
pub(crate) fn ident(name: &str) -> Identifier {
    ident_with(name, needs_quote(name))
}

pub(crate) fn ident_with(name: &str, quoted: bool) -> Identifier {
    if quoted {
        Identifier::quoted(name)
    } else {
        Identifier::new(name)
    }
}

/// The comparison key for an identifier: SQL folds only unquoted names, so
/// quoted identifiers compare exactly and unquoted ones case-insensitively.
pub(crate) fn ident_key(identifier: &Identifier) -> String {
    if identifier.quoted {
        identifier.name.clone()
    } else {
        identifier.name.to_lowercase()
    }
}

/// The name of an optional identifier, or `""`.
pub(crate) fn opt_name(identifier: &Option<Identifier>) -> &str {
    identifier.as_ref().map_or("", |i| i.name.as_str())
}

/// `catalog.schema.table` as written (without quotes), omitting absent parts.
pub(crate) fn qualified_name(table: &TableRef) -> String {
    let mut parts = Vec::with_capacity(3);
    for identifier in [&table.catalog, &table.schema].into_iter().flatten() {
        if !identifier.name.is_empty() {
            parts.push(identifier.name.as_str());
        }
    }
    parts.push(table.name.name.as_str());
    parts.join(".")
}

/// Whether `qualified` names the table `reference` on a dot boundary: equal,
/// or `qualified` ends with `.reference`. `raw.orders` matches
/// `hive.raw.orders` but not `hive.braw.orders`.
pub(crate) fn has_table_suffix(qualified: &str, reference: &str) -> bool {
    if reference.is_empty() {
        return false;
    }
    qualified == reference
        || (qualified.len() > reference.len()
            && qualified.ends_with(reference)
            && qualified.as_bytes()[qualified.len() - reference.len() - 1] == b'.')
}

pub(crate) fn table_names_match(a: &str, b: &str) -> bool {
    has_table_suffix(a, b) || has_table_suffix(b, a)
}

// ---------------------------------------------------------------------------
// Node identities and post-order edits
// ---------------------------------------------------------------------------

/// Stable identities for the nodes of an immutable tree.
///
/// Analysis passes borrow the tree and refer to nodes by address; [`NodeIds`]
/// translates those addresses into post-order positions, which is exactly
/// the order in which [`transform_all`] visits nodes. Edits planned against a
/// borrowed tree can therefore be applied to the owned tree afterwards.
pub(crate) struct NodeIds {
    ids: HashMap<*const Expression, usize>,
    kinds: Vec<Discriminant<Expression>>,
}

impl NodeIds {
    pub(crate) fn new(root: &Expression) -> Self {
        let mut ids = HashMap::new();
        let mut kinds = Vec::new();
        // Iterative post-order: (node, children already expanded).
        let mut stack: Vec<(&Expression, bool)> = vec![(root, false)];
        while let Some((node, expanded)) = stack.pop() {
            if expanded {
                ids.insert(node as *const Expression, kinds.len());
                kinds.push(discriminant(node));
                continue;
            }
            stack.push((node, true));
            let children = node.children();
            for child in children.into_iter().rev() {
                stack.push((child, false));
            }
        }
        Self { ids, kinds }
    }

    pub(crate) fn id(&self, node: &Expression) -> Option<usize> {
        self.ids.get(&(node as *const Expression)).copied()
    }
}

type Edit = Box<dyn FnOnce(Expression) -> Result<Expression>>;

/// A set of node replacements planned against a borrowed tree.
#[derive(Default)]
pub(crate) struct Edits {
    edits: HashMap<usize, Edit>,
}

impl Edits {
    /// Plans replacing `node` (a node of the tree `ids` was built from).
    pub(crate) fn replace(
        &mut self,
        ids: &NodeIds,
        node: &Expression,
        edit: impl FnOnce(Expression) -> Result<Expression> + 'static,
    ) -> Result<()> {
        let id = ids
            .id(node)
            .ok_or_else(|| Error::internal("edit target is not part of the statement"))?;
        if self.edits.insert(id, Box::new(edit)).is_some() {
            return Err(Error::internal("conflicting edits for one AST node"));
        }
        Ok(())
    }

    /// Applies the edits to the tree `ids` was built from. Fails closed if the
    /// traversal does not line up with the planned node identities.
    pub(crate) fn apply(self, root: Expression, ids: &NodeIds) -> Result<Expression> {
        let position = Cell::new(0usize);
        let edits = RefCell::new(self.edits);
        let failure = RefCell::new(None::<Error>);
        let result = transform_all(root, &|node| {
            let id = position.get();
            position.set(id + 1);
            if ids.kinds.get(id) != Some(&discriminant(&node)) {
                failure.replace(Some(Error::internal("AST traversal order mismatch")));
                return Ok(node);
            }
            let Some(edit) = edits.borrow_mut().remove(&id) else {
                return Ok(node);
            };
            match edit(node) {
                Ok(replacement) => Ok(replacement),
                Err(error) => {
                    failure.replace(Some(error));
                    Ok(Expression::Null(polyglot_sql::expressions::Null))
                }
            }
        })
        .map_err(|error| Error::internal(format!("AST transform failed: {error}")))?;
        if let Some(error) = failure.into_inner() {
            return Err(error);
        }
        if position.get() != ids.kinds.len() || !edits.borrow().is_empty() {
            return Err(Error::internal("AST traversal did not visit every planned edit"));
        }
        Ok(result)
    }
}
