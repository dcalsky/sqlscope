use polyglot_sql::expressions::{Identifier, MatchRecognize, Values, With};
use polyglot_sql::{Expression, Result};

use super::{lower_order, rendered};
use crate::Dialect;

pub(super) fn values_query(values: &Values, names: &[Identifier], target: Dialect) -> Result<String> {
    let mut rows = Vec::with_capacity(values.expressions.len());
    for (index, row) in values.expressions.iter().enumerate() {
        let mut columns = Vec::with_capacity(row.expressions.len());
        for (i, expr) in row.expressions.iter().enumerate() {
            let mut column = polyglot_sql::generate(expr, target.polyglot())?;
            if let Some(alias) = names.get(i).filter(|_| index == 0) {
                column.push_str(" AS ");
                column.push_str(&polyglot_sql::generate(
                    &Expression::Identifier(alias.clone()),
                    target.polyglot(),
                )?);
            }
            columns.push(column);
        }
        rows.push(format!("SELECT {}", columns.join(", ")));
    }
    Ok(rows.join(" UNION ALL "))
}

pub(super) fn cte_values(with: &mut Option<With>, target: Dialect) -> Result<()> {
    if let Some(with) = with {
        for cte in &mut with.ctes {
            if let Expression::Values(values) = &cte.this {
                let body = values_query(values, &[], target)?;
                cte.this = rendered(format!("SELECT * FROM ({body}) AS _values"));
            }
        }
    }
    Ok(())
}

/// The shared generator supports row-pattern syntax in its Trino renderer.
/// Render the expressions in the requested dialect before emitting that grammar.
pub(super) fn match_recognize(mr: &mut MatchRecognize, target: Dialect, strict: bool) -> Result<Expression> {
    let freeze = |e: &mut Expression| -> Result<()> {
        *e = rendered(polyglot_sql::generate(e, target.polyglot())?);
        Ok(())
    };
    if let Some(source) = &mut mr.this {
        freeze(source)?;
    }
    for expr in mr.partition_by.iter_mut().flatten() {
        freeze(expr)?;
    }
    if let Some(order) = &mut mr.order_by {
        lower_order(order, &[], strict)?;
        for key in order {
            freeze(&mut key.this)?;
        }
    }
    for measure in mr.measures.iter_mut().flatten() {
        freeze(&mut measure.this)?;
    }
    for (_, expr) in mr.define.iter_mut().flatten() {
        freeze(expr)?;
    }
    let tree = Expression::MatchRecognize(Box::new(mr.clone()));
    Ok(rendered(polyglot_sql::generate(&tree, Dialect::TRINO.polyglot())?))
}

/// Resolve only fixed, repeatable keys. Expanding a wildcard needs a schema,
/// and repeating volatile scalar expressions could sort on a different value.
pub(super) fn ordinal_key<'a>(ordinal: &str, projections: &'a [Expression]) -> Option<&'a Expression> {
    let position = ordinal.parse::<usize>().ok()?.checked_sub(1)?;
    let prefix = projections.get(..=position)?;
    if prefix.iter().any(|e| matches!(e, Expression::Star(_))) {
        return None;
    }
    let mut expr = prefix.last()?;
    if let Expression::Alias(alias) = expr {
        expr = &alias.this;
    }
    match expr {
        Expression::Column(_) => Some(expr),
        Expression::Count(count) if count.star && count.filter.is_none() => Some(expr),
        _ => None,
    }
}

pub(super) fn mysql_grouping(group: &mut polyglot_sql::expressions::GroupBy, strict: bool) -> Result<()> {
    let [Expression::Function(f)] = group.expressions.as_mut_slice() else {
        return Ok(());
    };
    if f.quoted || !f.qualified_name.is_empty() {
        return Ok(());
    }
    if f.name.eq_ignore_ascii_case("ROLLUP") && !f.args.is_empty() {
        let mut keys = std::mem::take(&mut f.args);
        keys.push(Expression::Rollup(Box::new(polyglot_sql::expressions::Rollup {
            expressions: vec![],
            options: None,
        })));
        group.expressions = keys;
    } else if strict && f.name.eq_ignore_ascii_case("CUBE") {
        return Err(polyglot_sql::Error::unsupported("GROUP BY CUBE", "mysql"));
    }
    Ok(())
}
