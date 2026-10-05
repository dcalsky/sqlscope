//! Independent AST oracles shared by the integration tests.
#![allow(dead_code, clippy::type_complexity)]

use std::collections::{BTreeMap, HashMap};

use polyglot_sql::expressions::Literal;
use polyglot_sql::{DialectType, Expression, ExpressionWalk};
use sqlscope::{Dialect, Options};

pub fn dialect(name: &str) -> Dialect {
    name.parse().expect("known dialect")
}

pub fn options(name: &str) -> Options {
    Options::new().dialect(dialect(name))
}

fn polyglot_dialect(name: &str) -> DialectType {
    let name = if name == "postgres" { "postgresql" } else { name };
    name.parse().expect("known dialect")
}

pub fn parse_one(sql: &str, dialect: &str) -> Expression {
    let mut statements = polyglot_sql::parse(sql, polyglot_dialect(dialect))
        .unwrap_or_else(|error| panic!("output does not parse: {error}\nSQL: {sql}"));
    assert_eq!(statements.len(), 1, "expected one statement: {sql}");
    statements.remove(0)
}

/// Counts string literals equal to `value`. Each wrap applies the predicate
/// once, so with a marker literal this equals the number of wrapped tables.
pub fn count_literals(sql: &str, dialect: &str, value: &str) -> usize {
    parse_one(sql, dialect)
        .dfs()
        .filter(
            |node| matches!(node, Expression::Literal(lit) if matches!(lit.as_ref(), Literal::String(s) if s == value)),
        )
        .count()
}

/// Whether a derived table in some FROM/JOIN lacks an alias.
pub fn has_unaliased_derived_table(sql: &str, dialect: &str) -> bool {
    parse_one(sql, dialect).dfs().any(|node| match node {
        Expression::Select(select) => select
            .from
            .iter()
            .flat_map(|f| f.expressions.iter())
            .chain(select.joins.iter().map(|j| &j.this))
            .any(|entry| matches!(entry, Expression::Subquery(sub) if sub.alias.as_ref().map_or(true, |a| a.name.is_empty()))),
        _ => false,
    })
}

/// Multiset of lower-cased physical table names.
pub fn table_counts(sql: &str, dialect: &str) -> Option<BTreeMap<String, usize>> {
    let statements = polyglot_sql::parse(sql, polyglot_dialect(dialect)).ok()?;
    let mut counts = BTreeMap::new();
    for statement in &statements {
        for node in statement.dfs() {
            if let Expression::Table(table) = node {
                if !table.name.name.is_empty() {
                    *counts.entry(table.name.name.to_lowercase()).or_insert(0) += 1;
                }
            }
        }
    }
    Some(counts)
}

fn entry_name(entry: &Expression) -> Option<String> {
    let name = match entry {
        Expression::Table(t) => t.alias.as_ref().unwrap_or(&t.name).name.clone(),
        Expression::Subquery(s) => s.alias.as_ref()?.name.clone(),
        Expression::Alias(a) => a.alias.name.clone(),
        _ => return None,
    };
    Some(name.to_lowercase())
}

/// Relation names bound by FROM/JOIN entries at every query level.
pub fn relation_names(sql: &str, dialect: &str) -> Vec<String> {
    let root = parse_one(sql, dialect);
    let mut names = Vec::new();
    for node in root.dfs() {
        if let Expression::Select(select) = node {
            for entry in select
                .from
                .iter()
                .flat_map(|f| f.expressions.iter())
                .chain(select.joins.iter().map(|j| &j.this))
            {
                names.extend(entry_name(entry));
            }
        }
    }
    names
}

/// The first relation name bound twice within a single FROM scope.
pub fn duplicate_in_scope(sql: &str, dialect: &str) -> Option<String> {
    let root = parse_one(sql, dialect);
    for node in root.dfs() {
        if let Expression::Select(select) = node {
            let mut seen = HashMap::new();
            for entry in select
                .from
                .iter()
                .flat_map(|f| f.expressions.iter())
                .chain(select.joins.iter().map(|j| &j.this))
            {
                if let Some(name) = entry_name(entry) {
                    if seen.insert(name.clone(), ()).is_some() {
                        return Some(name);
                    }
                }
            }
        }
    }
    None
}

pub fn map(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    entries
        .iter()
        .map(|(table, columns)| {
            let mut columns: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
            columns.sort();
            (table.to_string(), columns)
        })
        .collect()
}

/// Trino options with the users / orders / payments schema used by the
/// lineage tests.
pub fn trino_schema() -> Options {
    options("trino").schema([
        ("hive.raw.users", vec!["user_id", "user_name", "email"]),
        (
            "hive.raw.orders",
            vec![
                "order_id",
                "user_id",
                "amount",
                "quantity",
                "order_ts",
                "order_date",
                "status",
            ],
        ),
        ("hive.raw.payments", vec!["order_id", "paid_amount", "paid_at"]),
    ])
}
