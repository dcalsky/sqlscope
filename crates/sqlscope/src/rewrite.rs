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
use crate::options::Dialect;

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
        bare_alias(table)
    }

    /// Identifier keys of the unqualified tables the replacement itself
    /// reads. A CTE of the query with such a name would capture the read.
    fn unqualified_reads(&self) -> Vec<String> {
        Vec::new()
    }

    /// Builds the derived-table expression for `original`.
    fn build(&self, original: TableRef, alias: &AliasRef) -> Result<Expression>;
}

/// The name a table reference binds when unaliased. An Oracle database link
/// (`emp@remote`) binds the name before the `@`.
pub(crate) fn bare_alias(table: &TableRef) -> AliasRef {
    let mut alias = AliasRef::from_ident(&table.name);
    if !alias.quoted {
        if let Some((name, _)) = alias.name.split_once('@').filter(|(name, _)| !name.is_empty()) {
            alias.name = name.to_owned();
        }
    }
    alias
}

/// The identifier keys of the unqualified table references in `expression`.
pub(crate) fn unqualified_table_keys(expression: &Expression) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for node in expression.dfs() {
        if let Expression::Table(table) = node {
            if table.schema.is_none() && table.catalog.is_none() && !table.name.name.is_empty() {
                let key = ident_key(&table.name);
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
    }
    keys
}

/// Replaces every physical table reference for which `select` returns a
/// replacement. Returns `None` when nothing matched.
///
/// In BigQuery, a FROM path whose first part names a range variable already
/// in scope (`FROM t, t.arr`) reads a field of that relation, not a table,
/// and is left alone.
pub(crate) fn rewrite_statement<R: Replacement>(
    statement: Expression,
    dialect: Dialect,
    select: impl Fn(&TableRef) -> Option<R>,
) -> Result<Option<Expression>> {
    let ids = NodeIds::new(&statement);
    let edits = {
        let mut collector = Collector {
            correlated_paths: dialect.correlates_paths(),
            ..Collector::default()
        };
        collector.visit(&statement, &Scope::default());

        let mut matches: Vec<Match<'_, R>> = Vec::new();
        for site in collector.sites.iter().filter(|site| site.physical) {
            let bare = site.bare;
            let Some(replacement) = select(site.table) else {
                continue;
            };
            // The replacement is placed where the query's CTEs are visible;
            // a CTE must not stand in for a table the replacement reads.
            if let Some(name) = replacement
                .unqualified_reads()
                .into_iter()
                .find(|name| site.ctes.contains(name))
            {
                return Err(crate::Error::unsupported(format!(
                    "the query defines a CTE {name:?} that would replace a table the rewrite reads"
                )));
            }
            let mut matched = Match::new(site.node, site.table, replacement);
            matched.bare = bare;
            matches.push(matched);
        }
        if matches.is_empty() {
            return Ok(None);
        }

        assign_aliases(&mut matches, &collector.occupancy, dialect);

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
            let bare = matched.bare;
            edits.replace(&ids, matched.node, move |node| {
                let table = match node {
                    Expression::Table(table) => *table,
                    Expression::TableArgument(argument) => match argument.this {
                        Expression::Table(table) => *table,
                        _ => return Err(crate::Error::internal("table argument is not a table")),
                    },
                    _ => return Err(crate::Error::internal("replaced node is not a table")),
                };
                match replacement.build(table, &alias)? {
                    Expression::Subquery(mut subquery) if bare => {
                        subquery.alias = None;
                        subquery.alias_explicit_as = false;
                        subquery.column_aliases.clear();
                        Ok(Expression::Subquery(subquery))
                    }
                    built => Ok(built),
                }
            })?;
        }
        edits
    };
    let rewritten = edits.apply(statement, &ids)?;
    guard_chained_pivots(&rewritten)?;
    Ok(Some(rewritten))
}

/// The parser reads `t PIVOT (...) UNPIVOT (...)` but not the same chain
/// over an aliased subquery, which is what wrapping `t` produces.
fn guard_chained_pivots(statement: &Expression) -> Result<()> {
    fn source(expression: &Expression) -> Option<&Expression> {
        match expression {
            Expression::Pivot(pivot) => Some(&pivot.this),
            Expression::Unpivot(unpivot) => Some(&unpivot.this),
            _ => None,
        }
    }
    for node in statement.dfs() {
        let Some(inner) = source(node).filter(|inner| source(inner).is_some()) else {
            continue;
        };
        let mut innermost = inner;
        while let Some(next) = source(innermost) {
            innermost = next;
        }
        if matches!(innermost, Expression::Subquery(subquery) if subquery.alias.is_some()) {
            return Err(crate::Error::unsupported(
                "a filtered table under chained PIVOT / UNPIVOT clauses is not supported",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 1: collection
// ---------------------------------------------------------------------------

/// The CTE identifier keys, and the range variables a correlated path may
/// name, visible at a point of the statement.
#[derive(Clone, Default)]
struct Scope {
    ctes: HashSet<String>,
    relations: HashSet<String>,
}

impl Scope {
    fn with(&self, names: impl IntoIterator<Item = String>) -> Scope {
        let mut scope = self.clone();
        scope.ctes.extend(names);
        scope
    }

    fn with_relations<'e>(&self, entries: impl IntoIterator<Item = &'e Expression>) -> Scope {
        let mut scope = self.clone();
        scope.relations.extend(entries.into_iter().filter_map(relation_name));
        scope
    }
}

struct TableSite<'a> {
    /// The table node, or the `TABLE t` argument that holds it.
    node: &'a Expression,
    table: &'a TableRef,
    /// False for a CTE reference or a correlated path.
    physical: bool,
    /// The CTE names visible at the reference.
    ctes: HashSet<String>,
    /// The position takes no alias: a query argument (`TABLE t`) or the
    /// input of MATCH_RECOGNIZE.
    bare: bool,
}

#[derive(Default)]
struct Collector<'a> {
    correlated_paths: bool,
    sites: Vec<TableSite<'a>>,
    /// For each direct FROM/JOIN table entry: the relation names already bound
    /// in its FROM scope (visible CTEs and sibling relations).
    occupancy: HashMap<*const Expression, HashSet<String>>,
}

impl<'a> Collector<'a> {
    fn visit(&mut self, node: &'a Expression, scope: &Scope) {
        crate::ast::with_stack(|| {
            if is_query(node) {
                self.visit_query(node, scope);
                return;
            }
            // DESCRIBE reads a table's metadata, not its rows.
            if matches!(node, Expression::Describe(_)) {
                return;
            }
            if let Expression::TableArgument(argument) = node {
                // `MODEL m` names a model; `TABLE t` reads table t.
                let Expression::Table(table) = &argument.this else {
                    return;
                };
                if !argument.prefix.eq_ignore_ascii_case("TABLE") {
                    return;
                }
                self.record(node, table, scope, true);
                for child in argument.this.children() {
                    self.visit(child, scope);
                }
                return;
            }
            if let Expression::MatchRecognize(recognize) = node {
                if let Some(Expression::Table(table)) = recognize.this.as_deref() {
                    let input = recognize.this.as_deref().expect("checked above");
                    self.record(input, table, scope, true);
                    for child in node.children() {
                        if std::ptr::eq(child, input) {
                            child
                                .children()
                                .into_iter()
                                .for_each(|clause| self.visit(clause, scope));
                        } else {
                            self.visit(child, scope);
                        }
                    }
                    return;
                }
            }
            if let Expression::Table(table) = node {
                self.record(node, table, scope, false);
            }
            for child in node.children() {
                self.visit(child, scope);
            }
        })
    }

    /// Records a table reference; table-level clauses (TABLESAMPLE, time
    /// travel, hints) are visited by the caller, as they can hold subqueries.
    fn record(&mut self, node: &'a Expression, table: &'a TableRef, scope: &Scope, bare: bool) {
        if table.name.name.is_empty() {
            return;
        }
        // Only an unqualified name can denote a CTE.
        let unqualified = table.schema.is_none() && table.catalog.is_none();
        let cte_reference = unqualified && scope.ctes.contains(&ident_key(&table.name));
        let correlated = self.correlated_paths
            && table
                .catalog
                .as_ref()
                .or(table.schema.as_ref())
                .is_some_and(|first| scope.relations.contains(&ident_key(first)));
        self.sites.push(TableSite {
            node,
            table,
            physical: !cte_reference && !correlated,
            ctes: scope.ctes.clone(),
            bare,
        });
    }

    fn visit_query(&mut self, query: &'a Expression, scope: &Scope) {
        crate::ast::with_stack(|| {
            let mut cte_bodies: Vec<*const Expression> = Vec::new();
            let inner = match query_with(query) {
                Some(with) => {
                    let names: Vec<String> = with
                        .ctes
                        .iter()
                        .map(|cte| ident_key(&cte.alias))
                        .filter(|name| !name.is_empty())
                        .collect();
                    // A non-recursive CTE cannot see itself, so a same-named
                    // reference inside its body is the physical table. WITH
                    // RECURSIVE makes every sibling visible inside the bodies.
                    let mut body_scope = if with.recursive {
                        scope.with(names.iter().cloned())
                    } else {
                        scope.clone()
                    };
                    for cte in &with.ctes {
                        cte_bodies.push(&cte.this);
                        self.visit(&cte.this, &body_scope);
                        body_scope.ctes.insert(ident_key(&cte.alias));
                    }
                    scope.with(names)
                }
                None => scope.clone(),
            };

            let Expression::Select(select) = query else {
                for child in query.children() {
                    if !cte_bodies.contains(&(child as *const Expression)) {
                        self.visit(child, &inner);
                    }
                }
                return;
            };
            self.record_occupancy(select, &inner);
            // `SELECT ... INTO t, u` and `FOR UPDATE OF t` name targets or
            // relations of this query, not table reads.
            let targets: Vec<*const Expression> = select
                .into
                .iter()
                .flat_map(|into| std::iter::once(&into.this).chain(&into.expressions))
                .map(|e| e as *const Expression)
                .chain(
                    select
                        .locks
                        .iter()
                        .flat_map(|lock| lock.expressions.iter())
                        .map(|e| e as *const Expression),
                )
                .collect();
            let entries = from_entries(select);
            let clauses = inner.with_relations(entries.iter().copied());
            for child in query.children() {
                let pointer = child as *const Expression;
                if cte_bodies.contains(&pointer) || targets.contains(&pointer) {
                    continue;
                }
                match entries.iter().position(|entry| std::ptr::eq(*entry, child)) {
                    // A table path may correlate with the relations before it;
                    // a derived table only with enclosing queries.
                    Some(index) if matches!(child, Expression::Table(_)) => {
                        self.visit(child, &inner.with_relations(entries[..index].iter().copied()))
                    }
                    Some(_) => self.visit(child, &inner),
                    None => self.visit(child, &clauses),
                }
            }
        })
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

/// The identifier key a FROM entry binds: its alias, or the bare
/// table name.
pub(crate) fn relation_name(entry: &Expression) -> Option<String> {
    let name = match entry {
        Expression::Table(table) => ident_key(table.alias.as_ref().unwrap_or(&table.name)),
        Expression::Subquery(subquery) => ident_key(subquery.alias.as_ref()?),
        Expression::Alias(alias) => ident_key(&alias.alias),
        Expression::Unnest(unnest) => ident_key(unnest.alias.as_ref()?),
        Expression::Lateral(lateral) => lateral.alias.clone()?.to_lowercase(),
        Expression::Pivot(pivot) => ident_key(pivot.alias.as_ref()?),
        Expression::Unpivot(unpivot) => ident_key(unpivot.alias.as_ref()?),
        _ => return None,
    };
    (!name.is_empty()).then_some(name)
}

// ---------------------------------------------------------------------------
// Pass 2: alias assignment
// ---------------------------------------------------------------------------

struct Match<'a, R> {
    node: &'a Expression,
    table: &'a TableRef,
    replacement: R,
    /// Bare table identifier key, used for single-segment qualifiers.
    name_key: String,
    /// `catalog\0schema\0name` with SQL folding (quoted names exact), for
    /// unaliased references. Distinct identities get distinct aliases.
    identity: Option<String>,
    alias: AliasRef,
    /// Build the derived table without an alias.
    bare: bool,
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
            name_key: ident_key(&table.name),
            identity,
            alias,
            bare: false,
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
    dialect: Dialect,
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

    // A name valid as a table (`hive.raw.use`) may be a keyword where an
    // alias stands; such an alias is quoted.
    for alias in assigned.values_mut() {
        if !alias.quoted && !alias_parses(alias, dialect) {
            alias.quoted = true;
        }
    }

    for matched in matches.iter_mut() {
        if let Some(identity) = &matched.identity {
            matched.alias = assigned[identity].clone();
        }
    }
}

/// Whether `alias` parses as a derived-table alias when generated for
/// `dialect`.
fn alias_parses(alias: &AliasRef, dialect: Dialect) -> bool {
    let inner = Expression::Select(Box::new(select_from(vec![star()], TableRef::new("t"))));
    let probe = Expression::Select(Box::new(select_from_entry(
        vec![star()],
        derived_table(inner, alias, Vec::new()),
    )));
    crate::ast::generate(&probe, dialect).is_ok_and(|sql| crate::ast::parse(&sql, dialect).is_ok())
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
    /// Bare qualifiers inherited from enclosing scopes or replaced here.
    bare: Bindings,
    /// Relation names bound by this SELECT's FROM/JOIN entries.
    relations: HashSet<String>,
}

impl SelectScope {
    fn bindings(&self) -> Bindings {
        let mut bindings = self.qualified.clone();
        bindings.retain(|key, _| key.contains('\0'));
        bindings.extend(self.bare.clone());
        bindings
    }
}

impl<'r, 'a, R: Replacement> Rebinder<'r, 'a, R> {
    /// Rebinds within a query node, inheriting `qualified` from the enclosing
    /// query.
    fn query(&mut self, query: &'a Expression, qualified: &Bindings) -> Result<()> {
        crate::ast::with_stack(|| {
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
                bare: qualified
                    .iter()
                    .filter(|(key, _)| !key.contains('\0'))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                // Names as bound after the rewrite: a replaced entry binds its
                // derived-table alias.
                relations: entries
                    .iter()
                    .filter_map(
                        |entry| match self.matches.iter().find(|m| std::ptr::eq(m.node, *entry)) {
                            Some(matched) => Some(ident_key(&matched.alias.ident())),
                            None => relation_name(entry),
                        },
                    )
                    .collect(),
            };

            // Local relation names shadow inherited bare qualifiers, even if
            // the local relation is not replaced.
            for entry in &entries {
                if let Some(name) = relation_name(entry) {
                    scope.bare.remove(&name);
                }
            }

            let mut qualifier_counts: HashMap<String, usize> = HashMap::new();
            let mut name_counts: HashMap<&str, usize> = HashMap::new();
            for matched in &own {
                for key in matched.claimed_qualifiers() {
                    *qualifier_counts.entry(key).or_default() += 1;
                }
                *name_counts.entry(matched.name_key.as_str()).or_default() += 1;
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
                if name_counts[matched.name_key.as_str()] == 1 && ident_key(&matched.alias.ident()) != matched.name_key
                {
                    scope.bare.insert(matched.name_key.clone(), matched.alias.clone());
                }
            }

            let cte_bodies: Vec<*const Expression> = select
                .with
                .iter()
                .flat_map(|with| with.ctes.iter().map(|cte| &cte.this as *const Expression))
                .collect();
            for child in query.children() {
                if cte_bodies.contains(&(child as *const Expression)) {
                    // CTE bodies can see outer queries, but not this query's
                    // own FROM relations.
                    self.descend(child, qualified)?;
                } else {
                    self.clause(child, &scope)?;
                }
            }
            Ok(())
        })
    }

    /// Walks a node outside any SELECT clause, looking for nested queries.
    fn descend(&mut self, node: &'a Expression, qualified: &Bindings) -> Result<()> {
        crate::ast::with_stack(|| {
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
        })
    }

    /// Walks a node inside a SELECT clause, rebinding references.
    fn clause(&mut self, node: &'a Expression, scope: &SelectScope) -> Result<()> {
        crate::ast::with_stack(|| {
            if is_query(node) {
                return self.query(node, &scope.bindings());
            }
            match node {
                Expression::Subquery(subquery) if is_query(&subquery.this) => {
                    return self.query(&subquery.this, &scope.bindings());
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
        })
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
    select_from_entry(projection, Expression::Table(Box::new(table)))
}

fn select_from_entry(projection: Vec<Expression>, entry: Expression) -> Select {
    let mut select = Select::new();
    select.expressions = projection;
    select.from = Some(polyglot_sql::expressions::From {
        expressions: vec![entry],
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
