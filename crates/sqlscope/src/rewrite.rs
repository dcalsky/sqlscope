//! Scope-aware replacement of physical table references with derived tables.
//!
//! Shared by [`apply_row_filter`](crate::apply_row_filter) and
//! [`rewrite_tables`](crate::rewrite_tables). A rewrite runs in four passes
//! over one parsed statement:
//!
//! 1. **Collect** (read-only): walk the statement in source order with the
//!    CTE names visible at each point, and record every physical table
//!    reference. A bare name that denotes a visible CTE is not a physical table.
//! 2. **Alias**: give each replaced reference a derived-table alias. Explicit
//!    aliases are kept; unaliased references default to their bare name and
//!    are disambiguated when two distinct tables, or a sibling relation or
//!    CTE, would bind the same name in one FROM scope.
//! 3. **Rebind** (read-only): plan rewrites of column and star qualifiers that
//!    named a replaced table (`schema.table.col`, `table.*`, ...) so they keep
//!    resolving against the derived-table alias.
//! 4. **Apply**: perform the planned replacements on the owned tree.

use std::collections::{HashMap, HashSet};

use polyglot_sql::expressions::{Column, Identifier, Select, Star, Subquery, TableRef};
use polyglot_sql::{Expression, ExpressionWalk};

use crate::ast::{ident_key, ident_with, is_query, query_with, Edits, NodeIds};
use crate::error::Result;

/// An identifier name plus whether it must be quoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AliasRef {
    pub(crate) name: String,
    pub(crate) quoted: bool,
}

impl AliasRef {
    pub(crate) fn from_ident(identifier: &Identifier) -> Self {
        Self {
            name: identifier.name.clone(),
            quoted: identifier.quoted,
        }
    }

    pub(crate) fn ident(&self) -> Identifier {
        ident_with(&self.name, self.quoted)
    }
}

/// What a matched table reference is replaced with.
pub(crate) trait Replacement: Clone + 'static {
    /// The alias an unaliased reference prefers before collision handling.
    fn default_alias(&self, table: &TableRef) -> AliasRef {
        AliasRef::from_ident(&table.name)
    }

    /// Builds the derived-table expression for `original`.
    fn build(&self, original: TableRef, alias: &AliasRef) -> Result<Expression>;
}

/// Replaces every physical table reference for which `select` returns a
/// replacement. Returns `None` when nothing matched.
pub(crate) fn rewrite_statement<R: Replacement>(
    statement: Expression,
    select: impl Fn(&TableRef) -> Option<R>,
) -> Result<Option<Expression>> {
    let ids = NodeIds::new(&statement);
    let edits = {
        let mut collector = Collector::default();
        collector.visit(&statement, &Scope::default());

        let mut matches: Vec<Match<'_, R>> = collector
            .sites
            .iter()
            .filter(|site| !site.cte_reference)
            .filter_map(|site| select(site.table).map(|replacement| Match::new(site.node, site.table, replacement)))
            .collect();
        if matches.is_empty() {
            return Ok(None);
        }

        assign_aliases(&mut matches, &collector.occupancy);

        let mut edits = Edits::default();
        let mut rebinder = Rebinder {
            matches: &matches,
            ids: &ids,
            edits: &mut edits,
        };
        rebinder.query(&statement, &HashMap::new())?;

        for matched in &matches {
            let replacement = matched.replacement.clone();
            let alias = matched.alias.clone();
            edits.replace(&ids, matched.node, move |node| match node {
                Expression::Table(table) => replacement.build(*table, &alias),
                _ => Err(crate::Error::internal("replaced node is not a table")),
            })?;
        }
        edits
    };
    edits.apply(statement, &ids).map(Some)
}

// ---------------------------------------------------------------------------
// Pass 1: collection
// ---------------------------------------------------------------------------

/// The CTE names visible at a point of the statement (lower-case).
#[derive(Clone, Default)]
struct Scope {
    ctes: HashSet<String>,
}

impl Scope {
    fn with(&self, names: impl IntoIterator<Item = String>) -> Scope {
        let mut ctes = self.ctes.clone();
        ctes.extend(names);
        Scope { ctes }
    }
}

struct TableSite<'a> {
    node: &'a Expression,
    table: &'a TableRef,
    cte_reference: bool,
}

#[derive(Default)]
struct Collector<'a> {
    sites: Vec<TableSite<'a>>,
    /// For each direct FROM/JOIN table entry: the relation names already bound
    /// in its FROM scope (visible CTEs and sibling relations).
    occupancy: HashMap<*const Expression, HashSet<String>>,
}

impl<'a> Collector<'a> {
    fn visit(&mut self, node: &'a Expression, scope: &Scope) {
        if is_query(node) {
            self.visit_query(node, scope);
            return;
        }
        if let Expression::Table(table) = node {
            if !table.name.name.is_empty() {
                // Only an unqualified name can denote a CTE.
                let unqualified = table.schema.is_none() && table.catalog.is_none();
                let cte_reference = unqualified && scope.ctes.contains(&table.name.name.to_lowercase());
                self.sites.push(TableSite {
                    node,
                    table,
                    cte_reference,
                });
                return;
            }
        }
        for child in node.children() {
            self.visit(child, scope);
        }
    }

    fn visit_query(&mut self, query: &'a Expression, scope: &Scope) {
        let mut cte_bodies: Vec<*const Expression> = Vec::new();
        let inner = match query_with(query) {
            Some(with) => {
                let names: Vec<String> = with
                    .ctes
                    .iter()
                    .map(|cte| cte.alias.name.to_lowercase())
                    .filter(|name| !name.is_empty())
                    .collect();
                // A non-recursive CTE cannot see itself, so a same-named
                // reference inside its body is the physical table. WITH
                // RECURSIVE makes every sibling visible inside the bodies.
                let body_scope = if with.recursive {
                    scope.with(names.iter().cloned())
                } else {
                    scope.clone()
                };
                for cte in &with.ctes {
                    cte_bodies.push(&cte.this);
                    self.visit(&cte.this, &body_scope);
                }
                scope.with(names)
            }
            None => scope.clone(),
        };

        if let Expression::Select(select) = query {
            self.record_occupancy(select, &inner);
        }
        for child in query.children() {
            if !cte_bodies.contains(&(child as *const Expression)) {
                self.visit(child, &inner);
            }
        }
    }

    fn record_occupancy(&mut self, select: &'a Select, scope: &Scope) {
        let entries = from_entries(select);
        for entry in &entries {
            if !matches!(entry, Expression::Table(_)) {
                continue;
            }
            let mut occupied = scope.ctes.clone();
            for other in &entries {
                if !std::ptr::eq(*other, *entry) {
                    if let Some(name) = relation_name(other) {
                        occupied.insert(name);
                    }
                }
            }
            self.occupancy.insert(*entry as *const Expression, occupied);
        }
    }
}

/// The FROM and JOIN entries of a SELECT, in source order.
pub(crate) fn from_entries(select: &Select) -> Vec<&Expression> {
    let mut entries: Vec<&Expression> = select.from.iter().flat_map(|from| from.expressions.iter()).collect();
    entries.extend(select.joins.iter().map(|join| &join.this));
    entries
}

/// The lower-case relation name a FROM entry binds: its alias, or the bare
/// table name.
fn relation_name(entry: &Expression) -> Option<String> {
    let name = match entry {
        Expression::Table(table) => table
            .alias
            .as_ref()
            .filter(|alias| !alias.name.is_empty())
            .unwrap_or(&table.name)
            .name
            .clone(),
        Expression::Subquery(subquery) => subquery.alias.as_ref()?.name.clone(),
        Expression::Alias(alias) => alias.alias.name.clone(),
        Expression::Unnest(unnest) => unnest.alias.as_ref()?.name.clone(),
        Expression::Lateral(lateral) => lateral.alias.clone()?,
        Expression::Pivot(pivot) => pivot.alias.as_ref()?.name.clone(),
        Expression::Unpivot(unpivot) => unpivot.alias.as_ref()?.name.clone(),
        _ => return None,
    };
    (!name.is_empty()).then(|| name.to_lowercase())
}

// ---------------------------------------------------------------------------
// Pass 2: alias assignment
// ---------------------------------------------------------------------------

struct Match<'a, R> {
    node: &'a Expression,
    table: &'a TableRef,
    replacement: R,
    /// Lower-case bare table name, used for single-segment qualifiers.
    name_lower: String,
    /// `catalog\0schema\0name` with SQL folding (quoted names exact), for
    /// unaliased references. Distinct identities get distinct aliases.
    identity: Option<String>,
    alias: AliasRef,
}

impl<'a, R: Replacement> Match<'a, R> {
    fn new(node: &'a Expression, table: &'a TableRef, replacement: R) -> Self {
        let explicit = table.alias.as_ref().filter(|alias| !alias.name.is_empty());
        let identity = explicit.is_none().then(|| {
            let key = |identifier: &Option<Identifier>| identifier.as_ref().map(ident_key).unwrap_or_default();
            format!(
                "{}\0{}\0{}",
                key(&table.catalog),
                key(&table.schema),
                ident_key(&table.name)
            )
        });
        let alias = match explicit {
            Some(alias) => AliasRef::from_ident(alias),
            None => replacement.default_alias(table),
        };
        Self {
            node,
            table,
            replacement,
            name_lower: table.name.name.to_lowercase(),
            identity,
            alias,
        }
    }

    /// Multi-segment qualifiers (`schema.table`, `catalog.schema.table`) that
    /// unambiguously denote this unaliased reference.
    fn claimed_qualifiers(&self) -> Vec<String> {
        let Some(schema) = &self.table.schema else {
            return Vec::new();
        };
        let schema = ident_key(schema);
        let name = ident_key(&self.table.name);
        let mut qualifiers = vec![format!("{schema}\0{name}")];
        if let Some(catalog) = &self.table.catalog {
            qualifiers.push(format!("{}\0{schema}\0{name}", ident_key(catalog)));
        }
        qualifiers
    }
}

fn assign_aliases<R: Replacement>(
    matches: &mut [Match<'_, R>],
    occupancy: &HashMap<*const Expression, HashSet<String>>,
) {
    // Distinct unaliased identities in source order, with their preferred alias.
    let mut order: Vec<String> = Vec::new();
    let mut defaults: HashMap<String, AliasRef> = HashMap::new();
    for matched in matches.iter() {
        if let Some(identity) = &matched.identity {
            if !defaults.contains_key(identity) {
                order.push(identity.clone());
                defaults.insert(identity.clone(), matched.alias.clone());
            }
        }
    }

    let mut per_name: HashMap<String, usize> = HashMap::new();
    for identity in &order {
        *per_name.entry(defaults[identity].name.to_lowercase()).or_default() += 1;
    }

    let mut used: HashSet<String> = occupancy.values().flatten().cloned().collect();
    let mut needs_new_name: HashSet<&String> = HashSet::new();
    for identity in &order {
        let name = defaults[identity].name.to_lowercase();
        let collides_in_scope = matches.iter().any(|matched| {
            matched.identity.as_ref() == Some(identity)
                && occupancy
                    .get(&(matched.node as *const Expression))
                    .is_some_and(|occupied| occupied.contains(&name))
        });
        if per_name[&name] > 1 || collides_in_scope {
            needs_new_name.insert(identity);
        }
    }

    let mut assigned: HashMap<String, AliasRef> = HashMap::new();
    // Reserve the names that need no change first so new names avoid them.
    for identity in order.iter().filter(|identity| !needs_new_name.contains(identity)) {
        let alias = defaults[identity].clone();
        used.insert(alias.name.to_lowercase());
        assigned.insert(identity.clone(), alias);
    }
    for identity in order.iter().filter(|identity| needs_new_name.contains(identity)) {
        let alias = unique_alias(identity, &defaults[identity], &used);
        used.insert(alias.name.to_lowercase());
        assigned.insert(identity.clone(), alias);
    }

    for matched in matches.iter_mut() {
        if let Some(identity) = &matched.identity {
            matched.alias = assigned[identity].clone();
        }
    }
}

/// A unique alias for a colliding identity: `schema_table`, then
/// `catalog_schema_table`, then numeric suffixes.
fn unique_alias(identity: &str, default: &AliasRef, used: &HashSet<String>) -> AliasRef {
    let mut parts = identity.splitn(3, '\0');
    let catalog = parts.next().unwrap_or_default();
    let schema = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();

    let mut candidates = Vec::new();
    if !schema.is_empty() {
        candidates.push(format!("{schema}_{name}"));
        if !catalog.is_empty() {
            candidates.push(format!("{catalog}_{schema}_{name}"));
        }
    }
    let make = |name: String| AliasRef {
        quoted: crate::ast::needs_quote(&name),
        name,
    };
    if let Some(candidate) = candidates
        .into_iter()
        .find(|candidate| !used.contains(&candidate.to_lowercase()))
    {
        return make(candidate);
    }
    (2..)
        .map(|n| format!("{}_{n}", default.name))
        .find(|candidate| !used.contains(&candidate.to_lowercase()))
        .map(make)
        .expect("unbounded candidate sequence")
}

// ---------------------------------------------------------------------------
// Pass 3: column and star rebinding
// ---------------------------------------------------------------------------

/// Qualifier key (identifier keys joined by `\0`) -> derived alias.
type Bindings = HashMap<String, AliasRef>;

struct Rebinder<'r, 'a, R> {
    matches: &'r [Match<'a, R>],
    ids: &'r NodeIds,
    edits: &'r mut Edits,
}

struct SelectScope {
    /// Multi-segment qualifiers, inherited by nested queries.
    qualified: Bindings,
    /// Bare table names of this SELECT's own unaliased replaced tables.
    bare: Bindings,
    /// Relation names bound by this SELECT's FROM/JOIN entries.
    relations: HashSet<String>,
}

impl<'r, 'a, R: Replacement> Rebinder<'r, 'a, R> {
    /// Rebinds within a query node, inheriting `qualified` from the enclosing
    /// query.
    fn query(&mut self, query: &'a Expression, qualified: &Bindings) -> Result<()> {
        let Expression::Select(select) = query else {
            // Set operations have no FROM scope of their own.
            for child in query.children() {
                self.descend(child, qualified)?;
            }
            return Ok(());
        };

        let entries = from_entries(select);
        let own: Vec<&Match<'a, R>> = self
            .matches
            .iter()
            .filter(|matched| matched.identity.is_some())
            .filter(|matched| entries.iter().any(|entry| std::ptr::eq(*entry, matched.node)))
            .collect();

        let mut scope = SelectScope {
            qualified: qualified.clone(),
            bare: Bindings::new(),
            // Names as bound after the rewrite: a replaced entry binds its
            // derived-table alias.
            relations: entries
                .iter()
                .filter_map(
                    |entry| match self.matches.iter().find(|m| std::ptr::eq(m.node, *entry)) {
                        Some(matched) => Some(matched.alias.name.to_lowercase()),
                        None => relation_name(entry),
                    },
                )
                .collect(),
        };

        let mut qualifier_counts: HashMap<String, usize> = HashMap::new();
        let mut name_counts: HashMap<&str, usize> = HashMap::new();
        for matched in &own {
            for key in matched.claimed_qualifiers() {
                *qualifier_counts.entry(key).or_default() += 1;
            }
            *name_counts.entry(matched.name_lower.as_str()).or_default() += 1;
        }
        for matched in &own {
            for key in matched.claimed_qualifiers() {
                if qualifier_counts[&key] == 1 {
                    scope.qualified.insert(key, matched.alias.clone());
                } else {
                    // Ambiguous in this scope: callers must use the alias.
                    scope.qualified.remove(&key);
                }
            }
            // A bare `table.col` already resolves when the alias keeps the
            // bare name; several same-named tables make it ambiguous.
            if name_counts[matched.name_lower.as_str()] == 1 && matched.alias.name.to_lowercase() != matched.name_lower
            {
                scope.bare.insert(matched.name_lower.clone(), matched.alias.clone());
            }
        }

        let cte_bodies: Vec<*const Expression> = select
            .with
            .iter()
            .flat_map(|with| with.ctes.iter().map(|cte| &cte.this as *const Expression))
            .collect();
        for child in query.children() {
            if cte_bodies.contains(&(child as *const Expression)) {
                // CTE bodies are their own scopes; they only inherit qualified
                // bindings.
                self.descend(child, &scope.qualified)?;
            } else {
                self.clause(child, &scope)?;
            }
        }
        Ok(())
    }

    /// Walks a node outside any SELECT clause, looking for nested queries.
    fn descend(&mut self, node: &'a Expression, qualified: &Bindings) -> Result<()> {
        if is_query(node) {
            return self.query(node, qualified);
        }
        if let Expression::Subquery(subquery) = node {
            if is_query(&subquery.this) {
                return self.query(&subquery.this, qualified);
            }
        }
        for child in node.children() {
            self.descend(child, qualified)?;
        }
        Ok(())
    }

    /// Walks a node inside a SELECT clause, rebinding references.
    fn clause(&mut self, node: &'a Expression, scope: &SelectScope) -> Result<()> {
        if is_query(node) {
            return self.query(node, &scope.qualified);
        }
        match node {
            Expression::Subquery(subquery) if is_query(&subquery.this) => {
                return self.query(&subquery.this, &scope.qualified);
            }
            Expression::Star(star) => {
                if let Some(alias) = star_qualifier(star).and_then(|parts| lookup(&parts, scope)) {
                    let mut rebound = star.clone();
                    rebound.table = Some(alias.ident());
                    return self
                        .edits
                        .replace(self.ids, node, move |_| Ok(Expression::Star(rebound)));
                }
            }
            Expression::Column(_) | Expression::Dot(_) => {
                if let Some(chain) = reference_chain(node) {
                    if chain.len() >= 2 {
                        let qualifier: Vec<String> =
                            chain[..chain.len() - 1].iter().map(|part| ident_key(part)).collect();
                        if let Some(alias) = lookup(&qualifier, scope) {
                            let field = chain[chain.len() - 1].clone();
                            return self.edits.replace(self.ids, node, move |_| {
                                Ok(Expression::Column(Box::new(Column {
                                    name: field,
                                    table: Some(alias.ident()),
                                    join_mark: false,
                                    trailing_comments: Vec::new(),
                                    span: None,
                                    inferred_type: None,
                                })))
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        for child in node.children() {
            self.clause(child, scope)?;
        }
        Ok(())
    }
}

fn lookup(qualifier: &[String], scope: &SelectScope) -> Option<AliasRef> {
    match qualifier {
        [] => None,
        [bare] => {
            if scope.relations.contains(bare) {
                None
            } else {
                scope.bare.get(bare).cloned()
            }
        }
        parts => scope.qualified.get(&parts.join("\0")).cloned(),
    }
}

/// Flattens a column or dot-access reference into its identifiers.
fn reference_chain(node: &Expression) -> Option<Vec<&Identifier>> {
    match node {
        Expression::Column(column) => {
            let mut chain: Vec<&Identifier> = column.table.iter().collect();
            chain.push(&column.name);
            Some(chain)
        }
        Expression::Dot(dot) => {
            let mut chain = reference_chain(&dot.this)?;
            chain.push(&dot.field);
            Some(chain)
        }
        _ => None,
    }
}

/// The qualifier segments of `t.*` / `s.t.*`. The parser stores a
/// multi-part star qualifier as one dotted identifier; a quoted qualifier is
/// a single segment even when it contains dots.
fn star_qualifier(star: &Star) -> Option<Vec<String>> {
    let table = star.table.as_ref().filter(|table| !table.name.is_empty())?;
    if table.quoted {
        return Some(vec![ident_key(table)]);
    }
    Some(table.name.split('.').map(str::to_lowercase).collect())
}

// ---------------------------------------------------------------------------
// Derived-table construction helpers
// ---------------------------------------------------------------------------

pub(crate) fn star() -> Expression {
    Expression::Star(Star {
        table: None,
        except: None,
        replace: None,
        rename: None,
        trailing_comments: Vec::new(),
        span: None,
    })
}

pub(crate) fn column(name: &str) -> Expression {
    Expression::Column(Box::new(Column {
        name: crate::ast::ident(name),
        table: None,
        join_mark: false,
        trailing_comments: Vec::new(),
        span: None,
        inferred_type: None,
    }))
}

/// `SELECT <projection> FROM <table>`.
pub(crate) fn select_from(projection: Vec<Expression>, table: TableRef) -> Select {
    let mut select = Select::new();
    select.expressions = projection;
    select.from = Some(polyglot_sql::expressions::From {
        expressions: vec![Expression::Table(Box::new(table))],
    });
    select
}

/// `(<query>) AS <alias> [(<column aliases>)]`.
pub(crate) fn derived_table(query: Expression, alias: &AliasRef, column_aliases: Vec<Identifier>) -> Expression {
    Expression::Subquery(Box::new(Subquery {
        this: query,
        alias: Some(alias.ident()),
        column_aliases,
        alias_explicit_as: true,
        alias_keyword: None,
        order_by: None,
        limit: None,
        offset: None,
        distribute_by: None,
        sort_by: None,
        cluster_by: None,
        lateral: false,
        modifiers_inside: false,
        trailing_comments: Vec::new(),
        inferred_type: None,
    }))
}

/// The original reference stripped of its alias, for use inside a derived
/// table.
pub(crate) fn unaliased(mut table: TableRef) -> TableRef {
    table.alias = None;
    table.alias_explicit_as = false;
    table.column_aliases.clear();
    table
}
