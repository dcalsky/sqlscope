//! Scope-aware attribution of column references to root physical tables.
//!
//! The resolver walks each query scope, maps every relation in its FROM and
//! JOINs to a source (a physical table, or the resolved output of a CTE or
//! derived table), and attributes each column reference to the root tables it
//! reads. It is deliberately fail-open: a reference that cannot be resolved
//! precisely is attributed to every candidate table rather than dropped.
//!
//! In *reference* mode every referenced column is recorded together with the
//! clause it appears in. In *flow* mode nothing is recorded eagerly; callers
//! read the refs of the query's output, so only values that reach the result
//! count.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use polyglot_sql::expressions::{
    Column, Delete, Identifier, JoinKind, Literal, Merge, Ordered, Select, Star, Subquery, TableRef, Update, With,
};
use polyglot_sql::{Expression, ExpressionWalk};

use crate::ast::{is_query, qualified_name, table_names_match};
use crate::column_usages::{Clause, ColumnUsage};
use crate::schema::{self, Schema};

/// A `(root table, column)` pair.
pub(crate) type ColRef = (String, String);

/// The resolved output of a query scope: ordered output names, and the root
/// refs behind each name and each position.
#[derive(Clone, Debug, Default)]
pub(crate) struct Output {
    pub(crate) names: Vec<String>,
    pub(crate) by_name: HashMap<String, Vec<ColRef>>,
    pub(crate) positions: Vec<Vec<ColRef>>,
    /// For a SELECT (or a set operation, from its left branch): how many
    /// positions each projection item produced, in order. A star produces
    /// one per expanded column.
    pub(crate) items: Vec<usize>,
}

impl Output {
    fn add(&mut self, name: String, refs: Vec<ColRef>) {
        self.by_name
            .entry(name.to_lowercase())
            .or_default()
            .extend(refs.iter().cloned());
        self.names.push(name);
        self.positions.push(refs);
    }

    fn add_position(&mut self, refs: Vec<ColRef>) {
        self.positions.push(refs);
    }

    pub(crate) fn refs(&self, name: &str) -> Option<&Vec<ColRef>> {
        self.by_name.get(&name.to_lowercase())
    }

    /// Distinct root tables in first-seen order.
    fn roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = Vec::new();
        for (table, _) in self.positions.iter().flatten() {
            if !table.is_empty() && !roots.contains(table) {
                roots.push(table.clone());
            }
        }
        roots
    }
}

enum Source {
    Physical {
        table: String,
    },
    Derived {
        out: Rc<Output>,
        roots: Vec<String>,
    },
    /// A relation whose columns are unknown and read no table directly: a
    /// table function, UNNEST or lateral view.
    Opaque,
}

impl Source {
    fn roots(&self) -> Vec<String> {
        match self {
            Source::Physical { table } => vec![table.clone()],
            Source::Derived { roots, .. } => roots.clone(),
            Source::Opaque => Vec::new(),
        }
    }
}

/// How a joined relation shares columns with the relations before it.
#[derive(Clone)]
enum SharedColumns {
    /// `JOIN ... USING (k, ...)`.
    Using(Vec<String>),
    /// `NATURAL JOIN`: every column whose name is already present.
    Natural,
}

struct CteEntry<'a> {
    query: &'a Expression,
    column_aliases: Vec<String>,
    /// The scope the definition body resolves under.
    parent: RefCell<Option<Rc<Scope<'a>>>>,
    memo: RefCell<Option<Rc<Output>>>,
    /// Cycle guard for recursive CTEs.
    resolving: Cell<bool>,
}

#[derive(Default)]
struct Scope<'a> {
    sources: RefCell<Vec<Rc<Source>>>,
    by_alias: RefCell<HashMap<String, Rc<Source>>>,
    /// Source index -> the columns it shares with earlier sources, which an
    /// unqualified `*` outputs once.
    shared: RefCell<HashMap<usize, SharedColumns>>,
    ctes: HashMap<String, Rc<CteEntry<'a>>>,
    parent: Option<Rc<Scope<'a>>>,
    /// FROM-position wrappers (UNNEST, PIVOT arguments, table functions)
    /// collected once every relation of the scope is known.
    deferred: RefCell<Vec<&'a Expression>>,
}

impl<'a> Scope<'a> {
    fn push(&self, alias: Option<String>, source: Source) {
        let source = Rc::new(source);
        if let Some(alias) = alias.filter(|alias| !alias.is_empty()) {
            self.by_alias.borrow_mut().insert(alias, source.clone());
        }
        self.sources.borrow_mut().push(source);
    }

    fn physical_tables(&self) -> Vec<String> {
        self.sources
            .borrow()
            .iter()
            .filter_map(|source| match source.as_ref() {
                Source::Physical { table } => Some(table.clone()),
                Source::Derived { .. } | Source::Opaque => None,
            })
            .collect()
    }

    fn roots(&self) -> Vec<String> {
        let mut roots = Vec::new();
        for source in self.sources.borrow().iter() {
            for root in source.roots() {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        roots
    }

    /// Roots of this scope and every enclosing scope.
    fn chain_roots(self: &Rc<Self>) -> Vec<String> {
        let mut roots = Vec::new();
        let mut scope = Some(self.clone());
        while let Some(current) = scope {
            for root in current.roots() {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
            scope = current.parent.clone();
        }
        roots
    }
}

pub(crate) struct Resolver<'a, 's> {
    schema: &'s Schema,
    flow_only: bool,
    /// Root table -> referenced columns. Every physical table that appears in
    /// a FROM is present, possibly with no columns.
    pub(crate) result: BTreeMap<String, BTreeSet<String>>,
    pub(crate) usages: Option<BTreeSet<ColumnUsage>>,
    entries: Vec<Rc<CteEntry<'a>>>,
}

impl Drop for Resolver<'_, '_> {
    fn drop(&mut self) {
        // CTE entries and their body scopes reference each other.
        for entry in &self.entries {
            entry.parent.borrow_mut().take();
            entry.memo.borrow_mut().take();
        }
    }
}

fn refs_for(roots: &[String], name: &str) -> Vec<ColRef> {
    roots.iter().map(|root| (root.clone(), name.to_owned())).collect()
}

fn apply_column_aliases(out: Rc<Output>, aliases: &[String]) -> Rc<Output> {
    if aliases.is_empty() {
        return out;
    }
    let mut renamed = Output::default();
    for (index, alias) in aliases.iter().enumerate() {
        let refs = out
            .positions
            .get(index)
            .cloned()
            .or_else(|| out.names.get(index).and_then(|name| out.refs(name)).cloned())
            .unwrap_or_default();
        renamed.add(alias.clone(), refs);
    }
    Rc::new(renamed)
}

fn names(identifiers: &[Identifier]) -> Vec<String> {
    identifiers
        .iter()
        .map(|identifier| identifier.name.clone())
        .filter(|name| !name.is_empty())
        .collect()
}

/// A positive integer literal used as an output ordinal (`ORDER BY 1`).
fn ordinal(expression: &Expression) -> Option<usize> {
    match expression {
        Expression::Literal(literal) => match literal.as_ref() {
            Literal::Number(value) => value.parse::<usize>().ok().filter(|n| *n > 0),
            _ => None,
        },
        _ => None,
    }
}

/// `(column, qualifier)` of a dotted reference: `a.b.c.d` -> (`d`, `a.b.c`).
fn dot_parts(expression: &Expression) -> Option<(String, String)> {
    fn segments(expression: &Expression, out: &mut Vec<String>) -> bool {
        match expression {
            Expression::Dot(dot) => {
                if !segments(&dot.this, out) {
                    return false;
                }
                out.push(dot.field.name.clone());
                true
            }
            Expression::Column(column) => {
                if let Some(table) = &column.table {
                    out.push(table.name.clone());
                }
                out.push(column.name.name.clone());
                true
            }
            _ => false,
        }
    }
    let mut parts = Vec::new();
    if !segments(expression, &mut parts) || parts.is_empty() {
        return None;
    }
    let column = parts.pop()?;
    Some((column, parts.join(".")))
}

/// Unwraps a query that is parenthesized or an unaliased subquery.
fn as_query(expression: &Expression) -> Option<&Expression> {
    match expression {
        _ if is_query(expression) => Some(expression),
        Expression::Subquery(subquery) if subquery.alias.is_none() => as_query(&subquery.this),
        Expression::Paren(paren) => as_query(&paren.this),
        _ => None,
    }
}

impl<'a, 's> Resolver<'a, 's> {
    pub(crate) fn new(schema: &'s Schema, flow_only: bool, track_usages: bool) -> Self {
        Self {
            schema,
            flow_only,
            result: BTreeMap::new(),
            usages: track_usages.then(BTreeSet::new),
            entries: Vec::new(),
        }
    }

    fn add(&mut self, table: &str, column: &str, clause: Clause) {
        if self.flow_only || table.is_empty() || column.is_empty() {
            return;
        }
        self.result
            .entry(table.to_owned())
            .or_default()
            .insert(column.to_owned());
        if let Some(usages) = &mut self.usages {
            usages.insert(ColumnUsage {
                table: table.to_owned(),
                column: column.to_owned(),
                clause,
            });
        }
    }

    fn add_refs(&mut self, refs: &[ColRef], clause: Clause) {
        for (table, column) in refs {
            self.add(table, column, clause);
        }
    }

    /// Records refs regardless of mode; used by flow-mode callers for the
    /// refs that reach the result. The `*` sentinel is not a column.
    pub(crate) fn record_flow(&mut self, refs: &[ColRef]) {
        for (table, column) in refs {
            if !table.is_empty() && !column.is_empty() && column != "*" {
                self.result.entry(table.clone()).or_default().insert(column.clone());
            }
        }
    }

    // -----------------------------------------------------------------------
    // Scopes
    // -----------------------------------------------------------------------

    /// A scope whose visible CTEs are those of `with` plus the parent's.
    ///
    /// The query body sees every CTE of `with`. A definition body sees the
    /// enclosing CTEs plus, for a non-recursive WITH, only earlier siblings
    /// (a same-named reference in its own body is the physical table), or,
    /// for WITH RECURSIVE, every sibling.
    fn new_scope(&mut self, with: Option<&'a With>, parent: Option<&Rc<Scope<'a>>>) -> Rc<Scope<'a>> {
        let base: HashMap<String, Rc<CteEntry<'a>>> = parent.map(|parent| parent.ctes.clone()).unwrap_or_default();
        let mut all = base.clone();
        if let Some(with) = with {
            let mut ordered = Vec::new();
            for cte in &with.ctes {
                let name = cte.alias.name.to_lowercase();
                if name.is_empty() {
                    continue;
                }
                let entry = Rc::new(CteEntry {
                    query: &cte.this,
                    column_aliases: names(&cte.columns),
                    parent: RefCell::new(None),
                    memo: RefCell::new(None),
                    resolving: Cell::new(false),
                });
                self.entries.push(entry.clone());
                all.insert(name.clone(), entry.clone());
                ordered.push((name, entry));
            }
            let mut visible = base;
            for (name, entry) in ordered {
                let ctes = if with.recursive { all.clone() } else { visible.clone() };
                *entry.parent.borrow_mut() = Some(Rc::new(Scope {
                    ctes,
                    parent: parent.cloned(),
                    ..Scope::default()
                }));
                visible.insert(name, entry);
            }
        }
        Rc::new(Scope {
            ctes: all,
            parent: parent.cloned(),
            ..Scope::default()
        })
    }

    fn resolve_cte(&mut self, entry: &Rc<CteEntry<'a>>) -> Rc<Output> {
        if let Some(memo) = entry.memo.borrow().as_ref() {
            return memo.clone();
        }
        if entry.resolving.get() {
            return Rc::default();
        }
        entry.resolving.set(true);
        let parent = entry.parent.borrow().clone();
        let out = self.resolve_query(entry.query, parent.as_ref());
        let out = apply_column_aliases(out, &entry.column_aliases);
        entry.resolving.set(false);
        *entry.memo.borrow_mut() = Some(out.clone());
        out
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// Resolves a top-level query and returns its output mapping.
    pub(crate) fn resolve_root(&mut self, query: &'a Expression) -> Rc<Output> {
        self.resolve_query(query, None)
    }

    /// Resolves a SELECT or set operation and returns its output mapping.
    fn resolve_query(&mut self, query: &'a Expression, parent: Option<&Rc<Scope<'a>>>) -> Rc<Output> {
        crate::ast::with_stack(|| match query {
            Expression::Select(select) => self.resolve_select(select, parent),
            Expression::Union(op) => self.resolve_set_op(
                &op.left,
                &op.right,
                op.with.as_ref(),
                op.order_by.as_ref(),
                parent,
                true,
            ),
            Expression::Intersect(op) => self.resolve_set_op(
                &op.left,
                &op.right,
                op.with.as_ref(),
                op.order_by.as_ref(),
                parent,
                false,
            ),
            Expression::Except(op) => self.resolve_set_op(
                &op.left,
                &op.right,
                op.with.as_ref(),
                op.order_by.as_ref(),
                parent,
                false,
            ),
            _ => match as_query(query) {
                Some(inner) => self.resolve_query(inner, parent),
                None => Rc::default(),
            },
        })
    }

    fn resolve_select(&mut self, select: &'a Select, parent: Option<&Rc<Scope<'a>>>) -> Rc<Output> {
        crate::ast::with_stack(|| {
            let scope = self.new_scope(select.with.as_ref(), parent);
            for entry in select.from.iter().flat_map(|from| from.expressions.iter()) {
                self.add_source(entry, &scope);
            }
            for join in &select.joins {
                let first = scope.sources.borrow().len();
                self.add_source(&join.this, &scope);
                let shared = if matches!(
                    join.kind,
                    JoinKind::Natural | JoinKind::NaturalLeft | JoinKind::NaturalRight | JoinKind::NaturalFull
                ) {
                    Some(SharedColumns::Natural)
                } else if !join.using.is_empty() {
                    Some(SharedColumns::Using(names(&join.using)))
                } else {
                    None
                };
                if let Some(shared) = shared {
                    let added = scope.sources.borrow().len();
                    for index in first..added {
                        scope.shared.borrow_mut().insert(index, shared.clone());
                    }
                }
            }
            if !select.lateral_views.is_empty() {
                scope.push(None, Source::Opaque);
            }
            if !self.flow_only {
                self.collect_deferred(&scope);
            }

            let out = Rc::new(self.build_output(&select.expressions, &scope));
            // Filter, grouping and ordering positions do not flow into the result.
            if self.flow_only {
                return out;
            }

            if let Some(clause) = &select.where_clause {
                self.collect(&clause.this, &scope, Clause::Where, None);
            }
            if let Some(group_by) = &select.group_by {
                self.collect_items(group_by.expressions.iter(), &scope, Clause::GroupBy, &out);
            }
            if let Some(having) = &select.having {
                self.collect(&having.this, &scope, Clause::Having, Some(&out));
            }
            if let Some(qualify) = &select.qualify {
                self.collect(&qualify.this, &scope, Clause::Qualify, Some(&out));
            }
            for window in select.windows.iter().flatten() {
                for expression in &window.spec.partition_by {
                    self.collect(expression, &scope, Clause::Window, None);
                }
                for ordered in &window.spec.order_by {
                    self.collect(&ordered.this, &scope, Clause::Window, None);
                }
            }
            if let Some(order_by) = &select.order_by {
                self.collect_items(ordered_items(&order_by.expressions), &scope, Clause::OrderBy, &out);
            }
            if let Some(sort_by) = &select.sort_by {
                self.collect_items(ordered_items(&sort_by.expressions), &scope, Clause::SortBy, &out);
            }
            if let Some(distribute_by) = &select.distribute_by {
                self.collect_items(distribute_by.expressions.iter(), &scope, Clause::DistributeBy, &out);
            }
            if let Some(cluster_by) = &select.cluster_by {
                self.collect_items(ordered_items(&cluster_by.expressions), &scope, Clause::ClusterBy, &out);
            }
            if let Some(connect) = &select.connect {
                if let Some(start) = &connect.start {
                    self.collect(start, &scope, Clause::ConnectBy, None);
                }
                self.collect(&connect.connect, &scope, Clause::ConnectBy, None);
            }
            for view in &select.lateral_views {
                self.collect(&view.this, &scope, Clause::LateralView, None);
            }
            for join in &select.joins {
                if let Some(on) = &join.on {
                    self.collect(on, &scope, Clause::JoinOn, None);
                }
                // USING (k) names a column shared by both sides.
                for column in &join.using {
                    let refs = self.resolve_unqualified(&column.name, &scope);
                    self.add_refs(&refs, Clause::JoinUsing);
                }
            }
            out
        })
    }

    /// Merges set-operation branches positionally (names from the left).
    /// The right branch of INTERSECT / EXCEPT only filters left values, so in
    /// flow mode its values are dropped.
    fn resolve_set_op(
        &mut self,
        left: &'a Expression,
        right: &'a Expression,
        with: Option<&'a With>,
        order_by: Option<&'a polyglot_sql::expressions::OrderBy>,
        parent: Option<&Rc<Scope<'a>>>,
        right_values_flow: bool,
    ) -> Rc<Output> {
        let scope = self.new_scope(with, parent);
        let left = self.resolve_query(left, Some(&scope));
        let right = self.resolve_query(right, Some(&scope));
        let include_right = !self.flow_only || right_values_flow;

        let mut out = Output {
            names: left.names.clone(),
            items: left.items.clone(),
            ..Output::default()
        };
        for (index, name) in left.names.iter().enumerate() {
            let mut refs = left.refs(name).cloned().unwrap_or_default();
            if include_right {
                if let Some(right_refs) = right.names.get(index).and_then(|name| right.refs(name)) {
                    refs.extend(right_refs.iter().cloned());
                }
            }
            out.by_name.insert(name.to_lowercase(), refs);
        }
        let count = if include_right {
            left.positions.len().max(right.positions.len())
        } else {
            left.positions.len()
        };
        for index in 0..count {
            let mut refs = left.positions.get(index).cloned().unwrap_or_default();
            if include_right {
                refs.extend(right.positions.get(index).into_iter().flatten().cloned());
            }
            out.add_position(refs);
        }
        let out = Rc::new(out);
        if !self.flow_only {
            if let Some(order_by) = order_by {
                self.collect_items(ordered_items(&order_by.expressions), &scope, Clause::OrderBy, &out);
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // Sources
    // -----------------------------------------------------------------------

    fn add_source(&mut self, entry: &'a Expression, scope: &Rc<Scope<'a>>) {
        match entry {
            Expression::Table(table) => self.add_table(table, None, scope),
            Expression::Subquery(subquery) => self.add_subquery(subquery, scope),
            Expression::Pivot(pivot) => {
                self.add_source(&pivot.this, scope);
                let mut deferred = scope.deferred.borrow_mut();
                deferred.extend(pivot.expressions.iter());
                deferred.extend(pivot.fields.iter());
            }
            Expression::Unpivot(unpivot) => {
                self.add_source(&unpivot.this, scope);
                scope.deferred.borrow_mut().extend(unpivot.columns.iter());
            }
            Expression::Paren(paren) => self.add_source(&paren.this, scope),
            _ => {
                scope.deferred.borrow_mut().push(entry);
                scope.push(None, Source::Opaque);
            }
        }
    }

    fn add_subquery(&mut self, subquery: &'a Subquery, scope: &Rc<Scope<'a>>) {
        let out = self.resolve_query(&subquery.this, Some(scope));
        let out = apply_column_aliases(out, &names(&subquery.column_aliases));
        let alias = subquery.alias.as_ref().map(|alias| alias.name.to_lowercase());
        let roots = out.roots();
        scope.push(alias, Source::Derived { out, roots });
    }

    /// Registers a table reference. `extra_alias` is an alias the statement
    /// stores outside the table node (`DELETE FROM t AS x`).
    fn add_table(&mut self, table: &'a TableRef, extra_alias: Option<&Identifier>, scope: &Rc<Scope<'a>>) {
        let name = table.name.name.to_lowercase();
        if name.is_empty() {
            return;
        }
        let alias = table
            .alias
            .as_ref()
            .or(extra_alias)
            .map(|alias| alias.name.to_lowercase())
            .filter(|alias| !alias.is_empty())
            .unwrap_or_else(|| name.clone());

        let unqualified = table.schema.is_none() && table.catalog.is_none();
        if unqualified {
            if let Some(entry) = scope.ctes.get(&name).cloned() {
                let out = self.resolve_cte(&entry);
                let roots = out.roots();
                scope.push(Some(alias), Source::Derived { out, roots });
                return;
            }
        }
        let root = qualified_name(table);
        self.result.entry(root.clone()).or_default();
        scope.push(Some(alias), Source::Physical { table: root });
    }

    fn collect_deferred(&mut self, scope: &Rc<Scope<'a>>) {
        let deferred = std::mem::take(&mut *scope.deferred.borrow_mut());
        for expression in deferred {
            self.collect(expression, scope, Clause::From, None);
        }
    }

    // -----------------------------------------------------------------------
    // Projections
    // -----------------------------------------------------------------------

    fn build_output(&mut self, expressions: &'a [Expression], scope: &Rc<Scope<'a>>) -> Output {
        let mut out = Output::default();
        for item in expressions {
            let before = out.positions.len();
            match item {
                Expression::Star(star) => {
                    for (name, refs) in self.expand_star(star, scope) {
                        out.add(name, refs);
                    }
                }
                Expression::Alias(alias) => {
                    let refs = self.collect_refs(&alias.this, scope, Clause::Select);
                    out.add(alias.alias.name.clone(), refs);
                }
                Expression::Column(column) => {
                    let refs = self.collect_refs(item, scope, Clause::Select);
                    out.add(column.name.name.clone(), refs);
                }
                Expression::Dot(_) => {
                    let refs = self.collect_refs(item, scope, Clause::Select);
                    out.add(dot_parts(item).map(|(column, _)| column).unwrap_or_default(), refs);
                }
                _ => {
                    // An unaliased expression has an engine-defined name. In
                    // flow mode its refs still reach the caller (for example
                    // the value of a scalar subquery), under an empty name.
                    let refs = self.collect_refs(item, scope, Clause::Select);
                    if self.flow_only {
                        out.add(String::new(), refs);
                    } else {
                        out.add_position(refs);
                    }
                }
            }
            out.items.push(out.positions.len() - before);
        }
        out
    }

    /// Expands `*` / `t.*` into `(name, refs)` pairs, applying the star's
    /// EXCEPT / EXCLUDE, REPLACE and RENAME modifiers. An unknown relation
    /// yields the `*` sentinel.
    fn expand_star(&mut self, star: &'a Star, scope: &Rc<Scope<'a>>) -> Vec<(String, Vec<ColRef>)> {
        let mut columns = self.star_columns(star, scope);
        let matches = |identifier: &Identifier, name: &str| {
            if identifier.quoted {
                identifier.name == name
            } else {
                identifier.name.eq_ignore_ascii_case(name)
            }
        };
        if let Some(except) = &star.except {
            columns.retain(|(name, _)| name == "*" || !except.iter().any(|excluded| matches(excluded, name)));
        }
        let mut replaced = vec![false; columns.len()];
        for replacement in star.replace.iter().flatten() {
            if let Some(index) = columns.iter().position(|(name, _)| matches(&replacement.alias, name)) {
                // collect_refs records the replacement's own references.
                columns[index].1 = self.collect_refs(&replacement.this, scope, Clause::Select);
                replaced[index] = true;
            }
        }
        for (index, (_, refs)) in columns.iter().enumerate() {
            if !replaced[index] {
                self.add_refs(refs, Clause::Select);
            }
        }
        for (from, to) in star.rename.iter().flatten() {
            if let Some(column) = columns.iter_mut().find(|(name, _)| matches(from, name)) {
                column.0 = to.name.clone();
            }
        }
        columns
    }

    /// The unmodified columns a star stands for, without recording them.
    fn star_columns(&self, star: &Star, scope: &Rc<Scope<'a>>) -> Vec<(String, Vec<ColRef>)> {
        let mut out = Vec::new();
        let qualifier = star
            .table
            .as_ref()
            .map(|table| table.name.to_lowercase())
            .unwrap_or_default();

        let emit = |source: &Source, out: &mut Vec<(String, Vec<ColRef>)>| match source {
            Source::Derived { out: derived, .. } => {
                for name in &derived.names {
                    let refs = derived.refs(name).cloned().unwrap_or_default();
                    out.push((name.clone(), refs));
                }
            }
            Source::Physical { table } => match schema::columns(self.schema, table) {
                Some(columns) => {
                    for column in columns.iter().cloned() {
                        out.push((column.clone(), vec![(table.clone(), column)]));
                    }
                }
                None => out.push(("*".to_owned(), vec![(table.clone(), "*".to_owned())])),
            },
            Source::Opaque => out.push(("*".to_owned(), Vec::new())),
        };

        if qualifier.is_empty() {
            let shared = scope.shared.borrow();
            for (index, source) in scope.sources.borrow().iter().enumerate() {
                let Some(shared) = shared.get(&index) else {
                    emit(source, &mut out);
                    continue;
                };
                let mut columns = Vec::new();
                emit(source, &mut columns);
                for (name, refs) in columns {
                    let is_shared = name != "*"
                        && match shared {
                            SharedColumns::Using(using) => {
                                using.iter().any(|column| column.eq_ignore_ascii_case(&name))
                            }
                            SharedColumns::Natural => true,
                        };
                    // A shared column is output once; its value may come from
                    // either side.
                    match out
                        .iter_mut()
                        .find(|(existing, _)| is_shared && existing.eq_ignore_ascii_case(&name))
                    {
                        Some((_, existing)) => existing.extend(refs),
                        None => out.push((name, refs)),
                    }
                }
            }
            return out;
        }
        // The qualifier may name a relation of an enclosing scope (a
        // correlated `t.*` inside a subquery).
        let mut current = Some(scope.clone());
        while let Some(level) = current {
            let source = level.by_alias.borrow().get(&qualifier).cloned();
            if let Some(source) = source {
                emit(&source, &mut out);
                return out;
            }
            // A qualified star (`raw.users.*`) names a physical table.
            let mut matched: Vec<String> = Vec::new();
            for table in level.physical_tables() {
                if table_names_match(&qualifier, &table.to_lowercase()) && !matched.contains(&table) {
                    matched.push(table);
                }
            }
            if !matched.is_empty() {
                for table in matched {
                    emit(&Source::Physical { table }, &mut out);
                }
                return out;
            }
            current = level.parent.clone();
        }
        // Unknown qualifier: never drop it; broadcast the `*` sentinel.
        let mut roots = scope.chain_roots();
        if roots.is_empty() {
            roots.push(qualifier);
        }
        for root in roots {
            out.push(("*".to_owned(), vec![(root, "*".to_owned())]));
        }
        out
    }

    // -----------------------------------------------------------------------
    // Expressions
    // -----------------------------------------------------------------------

    /// Records every column reference in `expression`. With `output`,
    /// unqualified names that match a projection alias resolve to the
    /// projection's source columns (GROUP BY, HAVING, QUALIFY, ORDER BY ...).
    fn collect(&mut self, expression: &'a Expression, scope: &Rc<Scope<'a>>, clause: Clause, output: Option<&Output>) {
        crate::ast::with_stack(|| {
            if is_query(expression) {
                self.resolve_query(expression, Some(scope));
                return;
            }
            match expression {
                Expression::Column(column) => {
                    self.record_column(column, scope, clause, output);
                }
                Expression::Dot(_) => {
                    self.record_dot(expression, scope, clause);
                }
                _ => {
                    for child in expression.children() {
                        self.collect(child, scope, clause, output);
                    }
                }
            }
        })
    }

    /// Like [`collect`](Self::collect) for a clause whose items may be
    /// positive output ordinals (`GROUP BY 1`, `ORDER BY 2`).
    fn collect_items(
        &mut self,
        items: impl IntoIterator<Item = &'a Expression>,
        scope: &Rc<Scope<'a>>,
        clause: Clause,
        output: &Output,
    ) {
        for item in items {
            match ordinal(item) {
                Some(position) => {
                    if let Some(refs) = output.positions.get(position - 1) {
                        self.add_refs(&refs.clone(), clause);
                    }
                }
                None => self.collect(item, scope, clause, Some(output)),
            }
        }
    }

    /// Records the references in `expression` and returns them; a nested
    /// query contributes its output values.
    fn collect_refs(&mut self, expression: &'a Expression, scope: &Rc<Scope<'a>>, clause: Clause) -> Vec<ColRef> {
        crate::ast::with_stack(|| {
            if is_query(expression) {
                let out = self.resolve_query(expression, Some(scope));
                return out.positions.iter().flatten().cloned().collect();
            }
            match expression {
                Expression::Column(column) => self.record_column(column, scope, clause, None),
                Expression::Dot(_) => self.record_dot(expression, scope, clause),
                _ => {
                    let mut refs = Vec::new();
                    for child in expression.children() {
                        refs.extend(self.collect_refs(child, scope, clause));
                    }
                    refs
                }
            }
        })
    }

    fn record_column(
        &mut self,
        column: &Column,
        scope: &Rc<Scope<'a>>,
        clause: Clause,
        output: Option<&Output>,
    ) -> Vec<ColRef> {
        let name = column.name.name.as_str();
        if name.is_empty() {
            return Vec::new();
        }
        let qualifier = column.table.as_ref().map_or("", |table| table.name.as_str());
        if qualifier.is_empty() {
            if let Some(refs) = output.and_then(|output| output.refs(name)) {
                let refs = refs.clone();
                self.add_refs(&refs, clause);
                return refs;
            }
        }
        let refs = self.resolve(qualifier, name, scope);
        self.add_refs(&refs, clause);
        refs
    }

    fn record_dot(&mut self, expression: &Expression, scope: &Rc<Scope<'a>>, clause: Clause) -> Vec<ColRef> {
        let Some((column, qualifier)) = dot_parts(expression) else {
            return Vec::new();
        };
        if column.is_empty() {
            return Vec::new();
        }
        let refs = self.resolve(&qualifier, &column, scope);
        self.add_refs(&refs, clause);
        refs
    }

    // -----------------------------------------------------------------------
    // Attribution
    // -----------------------------------------------------------------------

    fn resolve(&self, qualifier: &str, name: &str, scope: &Rc<Scope<'a>>) -> Vec<ColRef> {
        if qualifier.is_empty() {
            self.resolve_unqualified(name, scope)
        } else {
            Self::resolve_qualified(&qualifier.to_lowercase(), name, scope)
        }
    }

    fn resolve_qualified(qualifier: &str, name: &str, scope: &Rc<Scope<'a>>) -> Vec<ColRef> {
        let mut current = Some(scope.clone());
        while let Some(level) = current {
            let source = level.by_alias.borrow().get(qualifier).cloned();
            if let Some(source) = source {
                return match source.as_ref() {
                    Source::Physical { table } => vec![(table.clone(), name.to_owned())],
                    // A name missing from a derived output (an unexpanded
                    // star) is attributed to the derived source's roots.
                    Source::Derived { out, roots } => out.refs(name).cloned().unwrap_or_else(|| refs_for(roots, name)),
                    // Never registered under an alias; fail open regardless.
                    Source::Opaque => refs_for(&scope.chain_roots(), name),
                };
            }
            let mut matched: Vec<String> = Vec::new();
            for table in level.physical_tables() {
                if table_names_match(qualifier, &table.to_lowercase()) && !matched.contains(&table) {
                    matched.push(table);
                }
            }
            if !matched.is_empty() {
                return refs_for(&matched, name);
            }
            current = level.parent.clone();
        }
        // Unknown qualifier: attribute to every table in scope.
        let roots = scope.chain_roots();
        if roots.is_empty() {
            vec![(qualifier.to_owned(), name.to_owned())]
        } else {
            refs_for(&roots, name)
        }
    }

    fn resolve_unqualified(&self, name: &str, scope: &Rc<Scope<'a>>) -> Vec<ColRef> {
        if name.is_empty() {
            return Vec::new();
        }
        let mut refs = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        // Whether some source exposes the name. A derived column computed
        // without column inputs (`count(*) AS n`) has no refs but still binds
        // the name, so it must not fall through to the fallbacks below.
        let mut bound = false;
        for source in scope.sources.borrow().iter() {
            match source.as_ref() {
                Source::Derived { out, roots } => {
                    if let Some(found) = out.refs(name) {
                        bound = true;
                        refs.extend(found.iter().cloned());
                    } else if out.by_name.contains_key("*") {
                        // An unexpanded star may expose the name.
                        unknown.extend(roots.iter().cloned());
                    }
                }
                Source::Physical { table } => match schema::columns(self.schema, table) {
                    Some(columns) => {
                        if columns.iter().any(|column| column == name) {
                            refs.push((table.clone(), name.to_owned()));
                        }
                    }
                    None => unknown.push(table.clone()),
                },
                // Columns of table functions are not attributed to tables.
                Source::Opaque => {}
            }
        }
        let mut distinct: Vec<String> = Vec::new();
        for table in unknown {
            if !distinct.contains(&table) {
                distinct.push(table);
            }
        }
        refs.extend(refs_for(&distinct, name));
        if bound || !refs.is_empty() {
            return refs;
        }
        // Every current source is fully known and none has the name: SQL
        // correlation resolves it against enclosing scopes.
        if let Some(parent) = &scope.parent {
            let found = self.resolve_unqualified(name, parent);
            if !found.is_empty() {
                return found;
            }
        }
        refs_for(&scope.roots(), name)
    }

    // -----------------------------------------------------------------------
    // DML
    // -----------------------------------------------------------------------

    pub(crate) fn resolve_delete(&mut self, delete: &'a Delete) {
        let scope = self.new_scope(delete.with.as_ref(), None);
        self.add_table(&delete.table, delete.alias.as_ref(), &scope);
        for join in &delete.joins {
            self.add_source(&join.this, &scope);
        }
        for table in &delete.using {
            self.add_table(table, None, &scope);
        }
        self.collect_deferred(&scope);
        if let Some(clause) = &delete.where_clause {
            self.collect(&clause.this, &scope, Clause::Where, None);
        }
        self.collect_joins(&delete.joins, &scope);
        if let Some(order_by) = &delete.order_by {
            for item in ordered_items(&order_by.expressions) {
                self.collect(item, &scope, Clause::OrderBy, None);
            }
        }
    }

    fn update_sources(&mut self, update: &'a Update) -> Rc<Scope<'a>> {
        let scope = self.new_scope(update.with.as_ref(), None);
        self.add_table(&update.table, None, &scope);
        for table in &update.extra_tables {
            self.add_table(table, None, &scope);
        }
        for join in update.table_joins.iter().chain(&update.from_joins) {
            self.add_source(&join.this, &scope);
        }
        for entry in update.from_clause.iter().flat_map(|from| from.expressions.iter()) {
            self.add_source(entry, &scope);
        }
        scope
    }

    pub(crate) fn resolve_update(&mut self, update: &'a Update) {
        let scope = self.update_sources(update);
        self.collect_deferred(&scope);
        let target = qualified_name(&update.table);
        for (column, value) in &update.set {
            // Some dialects store a qualified target `t.x` as one identifier.
            match column.name.rsplit_once('.') {
                Some((qualifier, name)) => {
                    let refs = Self::resolve_qualified(&qualifier.to_lowercase(), name, &scope);
                    self.add_refs(&refs, Clause::UpdateSetTarget);
                }
                None => self.add(&target, &column.name, Clause::UpdateSetTarget),
            }
            self.collect(value, &scope, Clause::UpdateSetValue, None);
        }
        if let Some(clause) = &update.where_clause {
            self.collect(&clause.this, &scope, Clause::Where, None);
        }
        self.collect_joins(&update.table_joins, &scope);
        self.collect_joins(&update.from_joins, &scope);
        if let Some(order_by) = &update.order_by {
            for item in ordered_items(&order_by.expressions) {
                self.collect(item, &scope, Clause::OrderBy, None);
            }
        }
    }

    fn merge_sources(&mut self, merge: &'a Merge) -> Rc<Scope<'a>> {
        let with = match merge.with_.as_deref() {
            Some(Expression::With(with)) => Some(with.as_ref()),
            _ => None,
        };
        let scope = self.new_scope(with, None);
        self.add_source(&merge.this, &scope);
        self.add_source(&merge.using, &scope);
        scope
    }

    pub(crate) fn resolve_merge(&mut self, merge: &'a Merge) {
        let scope = self.merge_sources(merge);
        self.collect_deferred(&scope);
        if let Some(on) = &merge.on {
            self.collect(on, &scope, Clause::MergeOn, None);
        }
        if let Some(whens) = &merge.whens {
            self.collect(whens, &scope, Clause::MergeWhen, None);
        }
    }

    fn collect_joins(&mut self, joins: &'a [polyglot_sql::expressions::Join], scope: &Rc<Scope<'a>>) {
        for join in joins {
            if let Some(on) = &join.on {
                self.collect(on, scope, Clause::JoinOn, None);
            }
            for column in &join.using {
                let refs = self.resolve_unqualified(&column.name, scope);
                self.add_refs(&refs, Clause::JoinUsing);
            }
        }
    }

    /// Value flow of `UPDATE ... SET`: only assigned values count.
    pub(crate) fn update_value_flow(&mut self, update: &'a Update) {
        let scope = self.update_sources(update);
        for (_, value) in &update.set {
            let refs = self.collect_refs(value, &scope, Clause::UpdateSetValue);
            self.record_flow(&refs);
        }
    }

    /// Value flow of `MERGE`: only values assigned by UPDATE / INSERT actions
    /// count; match conditions and target columns do not.
    pub(crate) fn merge_value_flow(&mut self, merge: &'a Merge) {
        let scope = self.merge_sources(merge);
        for value in merge_assignment_values(merge.whens.as_deref()) {
            let refs = self.collect_refs(value, &scope, Clause::MergeWhen);
            self.record_flow(&refs);
        }
    }
}

fn ordered_items(items: &[Ordered]) -> impl Iterator<Item = &Expression> {
    items.iter().map(|ordered| &ordered.this)
}

/// Values written by MERGE actions: `UPDATE SET x = <value>` and
/// `INSERT (...) VALUES (<values>)`.
fn merge_assignment_values(whens: Option<&Expression>) -> Vec<&Expression> {
    let mut values = Vec::new();
    let Some(Expression::Whens(whens)) = whens else {
        return values;
    };
    for when in &whens.expressions {
        let Expression::When(when) = when else { continue };
        let Expression::Tuple(action) = when.then.as_ref() else {
            continue;
        };
        let parts = &action.expressions;
        if parts.len() < 2 {
            continue;
        }
        let kind = match &parts[0] {
            Expression::Var(var) => var.this.to_ascii_uppercase(),
            _ => continue,
        };
        match kind.as_str() {
            "UPDATE" => {
                if let Expression::Tuple(assignments) = &parts[1] {
                    for assignment in &assignments.expressions {
                        if let Expression::Eq(eq) = assignment {
                            values.push(&eq.right);
                        }
                    }
                }
            }
            "INSERT" => {
                // Target columns are in the penultimate tuple, values last.
                if let Some(Expression::Tuple(row)) = parts.last() {
                    values.extend(row.expressions.iter());
                }
            }
            _ => {}
        }
    }
    values
}
