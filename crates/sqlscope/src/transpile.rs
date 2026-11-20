//! Conversion from Trino SQL to the SQLGlot-compatible target spelling.
mod ddl;
mod functions;
mod query;
mod source;
mod time;

use polyglot_sql::expressions::{Case, DataType, Function, GroupConcatFunc, IsNull, Literal, Ordered};
use polyglot_sql::{traversal::transform_all, Expression};

use crate::{ast, Dialect, Error, Result};

/// Controls conversion of constructs that have no supported target spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranspileMode {
    Strict,
    BestEffort,
}

/// Parses a Trino expression or script and returns one SQL string per statement.
/// The supported targets are StarRocks and MySQL.
pub fn transpile(sql: &str, source: Dialect, target: Dialect, mode: TranspileMode) -> Result<Vec<String>> {
    if source != Dialect::TRINO {
        return Err(Error::invalid("expected trino source dialect"));
    }
    if !matches!(target, Dialect::STARROCKS | Dialect::MYSQL) {
        return Err(Error::invalid("expected starrocks or mysql target"));
    }
    ast::guard_input(sql)?;
    ast::with_large_stack(|| {
        let source = source::parse_source(sql)?;
        let statements = ast::parse_raw(&source.sql, Dialect::TRINO)??;
        statements
            .into_iter()
            .map(|statement| {
                let lowered = transform_all(statement, &|expr| lower(source.restore(expr, target)?, target, mode))
                    .map_err(|e| Error::unsupported(e.to_string()))?;
                let output = ast::generate(&lowered, target)?;
                ast::guard_input(&output)?;
                Ok(output)
            })
            .collect()
    })
}

fn lower(mut expr: Expression, target: Dialect, mode: TranspileMode) -> polyglot_sql::Result<Expression> {
    let strict = mode == TranspileMode::Strict;
    if let Expression::WithFunction(routine) = expr {
        if strict {
            return Err(polyglot_sql::Error::unsupported(
                "inline SQL functions",
                target.to_string(),
            ));
        }
        return Ok(routine.this);
    }
    if let Expression::Function(f) = expr {
        if f.qualified_name.is_empty() && !f.quoted {
            return functions::lower(f, target, strict);
        }
        return Ok(Expression::Function(f));
    }
    match &mut expr {
        Expression::CreateTable(_)
        | Expression::CreateSchema(_)
        | Expression::AlterTable(_)
        | Expression::AlterView(_)
        | Expression::Analyze(_) => return ddl::statement(expr, target, mode),
        Expression::Literal(lit) => match lit.as_ref() {
            Literal::String(s) if target == Dialect::STARROCKS && s.contains(['\\', '\n', '\r', '\t', '\0']) => {
                return Ok(rendered(polyglot_sql::generate(&expr, Dialect::MYSQL.polyglot())?));
            }
            Literal::Time(value) => return Ok(time::literal(value, false)),
            Literal::Timestamp(value) => return Ok(time::literal(value, true)),
            _ => {}
        },
        Expression::DataType(ty @ DataType::Array { .. }) if target == Dialect::MYSQL => {
            return Ok(Expression::DataType(DataType::Custom {
                name: mysql_array_element(ty)?,
            }));
        }
        Expression::Length(f) => return Ok(call("CHAR_LENGTH", vec![f.this.clone()])),
        Expression::DayOfWeek(f) => return Ok(call("DAYOFWEEK_ISO", vec![f.this.clone()])),
        Expression::WeekOfYear(f) if target == Dialect::STARROCKS => {
            return Ok(call("WEEKOFYEAR", vec![f.this.clone()]))
        }
        Expression::Concat(op) => return Ok(call("CONCAT", vec![op.left.clone(), op.right.clone()])),
        Expression::ArrayFunc(array) => {
            if target == Dialect::MYSQL {
                if strict {
                    return Err(polyglot_sql::Error::unsupported("arrays", "mysql"));
                }
                return Ok(call("ARRAY", std::mem::take(&mut array.expressions)));
            }
            array.bracket_notation = true;
        }
        Expression::Cast(cast) | Expression::TryCast(cast) => match cast.to {
            DataType::Int { .. } if target == Dialect::MYSQL => cast.to = DataType::Custom { name: "SIGNED".into() },
            DataType::Float {
                ref mut real_spelling, ..
            } if target == Dialect::STARROCKS => *real_spelling = false,
            DataType::Array { ref element_type, .. } if target == Dialect::MYSQL => {
                let ty = mysql_array_element(element_type)?;
                cast.to = DataType::Custom {
                    name: format!("ARRAY<{ty}>"),
                };
            }
            DataType::Timestamp { timezone: true, .. } => {
                return Ok(call("TIMESTAMP", vec![cast.this.clone()]));
            }
            DataType::Time {
                timezone: true,
                precision,
            } => {
                let name = precision.map_or("TIMETZ".into(), |p| format!("TIMETZ({p})"));
                cast.to = DataType::Custom { name };
            }
            _ => {}
        },
        Expression::JsonExtract(f) if target == Dialect::STARROCKS => f.arrow_syntax = true,
        Expression::JsonExtractScalar(f) => f.arrow_syntax = true,
        Expression::JsonQuery(f) => {
            if target == Dialect::STARROCKS {
                let mut f = f.clone();
                f.wrapper_option = None;
                f.quotes_option = None;
                f.on_scalar_string = false;
                f.on_error = None;
                f.arrow_syntax = true;
                return Ok(Expression::JsonExtract(f));
            }
            let mut args = vec![f.this.clone(), f.path.clone()];
            for mut option in [f.wrapper_option.take(), f.quotes_option.take(), f.on_error.take()]
                .into_iter()
                .flatten()
            {
                if option.ends_with("QUOTES") && f.on_scalar_string {
                    option.push_str(" ON SCALAR STRING");
                }
                args.push(Expression::Raw(polyglot_sql::expressions::Raw { sql: option }));
            }
            return Ok(call("JSON_EXTRACT", args));
        }
        Expression::AtTimeZone(z) => {
            if strict {
                return Err(polyglot_sql::Error::unsupported("AT TIME ZONE", target.to_string()));
            }
            return Ok(z.this.clone());
        }
        Expression::CreateView(view) if target == Dialect::MYSQL && view.security.is_some() => {
            view.security_sql_style = true;
            view.security_after_name = false;
        }
        Expression::Table(t) if target == Dialect::MYSQL => {
            for id in [Some(&mut t.name), t.schema.as_mut(), t.catalog.as_mut()]
                .into_iter()
                .flatten()
            {
                if id.name.eq_ignore_ascii_case("system") {
                    id.quoted = true;
                }
            }
        }
        Expression::FirstValue(f) | Expression::LastValue(f) if f.ignore_nulls.is_some() => {
            if target == Dialect::STARROCKS {
                if f.ignore_nulls.take() == Some(true) {
                    let arg = polyglot_sql::generate(&f.this, target.polyglot())?;
                    f.this = rendered(format!("{arg} IGNORE NULLS"));
                }
            } else if f.ignore_nulls == Some(true) {
                if strict {
                    return Err(polyglot_sql::Error::unsupported("IGNORE NULLS", "mysql"));
                }
                f.ignore_nulls = None;
            }
        }
        Expression::WindowFunction(w) => lower_order(&mut w.over.order_by, &[], strict)?,
        Expression::Window(w) => lower_order(&mut w.order_by, &[], strict)?,
        Expression::Transaction(tx) => tx.mark = None,
        Expression::Trim(trim) if trim.characters.is_some() => {
            use polyglot_sql::expressions::TrimPosition::*;
            if target == Dialect::STARROCKS {
                let name = match trim.position {
                    Both => "TRIM",
                    Leading => "LTRIM",
                    Trailing => "RTRIM",
                };
                return Ok(call(name, vec![trim.this.clone(), trim.characters.take().unwrap()]));
            }
            trim.sql_standard_syntax = true;
        }
        Expression::MatchRecognize(mr) => return query::match_recognize(mr, target, strict),
        Expression::Subquery(sub) => {
            if let Expression::Values(values) = &sub.this {
                sub.this = rendered(query::values_query(values, &sub.column_aliases, target)?);
                sub.column_aliases.clear();
            }
        }
        Expression::Column(col)
            if target == Dialect::MYSQL && !col.name.quoted && col.name.name.eq_ignore_ascii_case("text") =>
        {
            let mut name = String::new();
            if let Some(table) = &col.table {
                name = polyglot_sql::generate(&Expression::Identifier(table.clone()), target.polyglot())?;
                name.push('.');
            }
            name.push_str(&col.name.name);
            return Ok(rendered(name));
        }
        Expression::When(when) => {
            if let Expression::Tuple(action) = when.then.as_mut() {
                if let [Expression::Var(kind), Expression::Tuple(assignments)] = action.expressions.as_mut_slice() {
                    if kind.this == "UPDATE" {
                        for assignment in &mut assignments.expressions {
                            if let Expression::Eq(eq) = assignment {
                                eq.right = rendered(polyglot_sql::generate(&eq.right, target.polyglot())?);
                            }
                        }
                    }
                }
            }
        }
        Expression::Select(select) => {
            query::cte_values(&mut select.with, target)?;
            if target == Dialect::MYSQL {
                if let Some(group) = &mut select.group_by {
                    query::mysql_grouping(group, strict)?;
                }
            }
            if let Some(fetch) = select.fetch.take() {
                select.limit = Some(polyglot_sql::expressions::Limit {
                    this: fetch.count.unwrap_or_else(|| Expression::number(1)),
                    percent: fetch.percent,
                    comments: vec![],
                });
            }
            if let Some(offset) = &mut select.offset {
                offset.rows = None;
            }
            if let Some(order) = &mut select.order_by {
                lower_order(&mut order.expressions, &select.expressions, strict)?;
            }
        }
        Expression::Intersect(set) => {
            query::cte_values(&mut set.with, target)?;
            if let Some(order) = &mut set.order_by {
                lower_order(&mut order.expressions, &[], strict)?;
            }
        }
        Expression::Except(set) => {
            query::cte_values(&mut set.with, target)?;
            if target == Dialect::STARROCKS && set.all && strict {
                return Err(polyglot_sql::Error::unsupported("EXCEPT ALL", "starrocks"));
            }
            if let Some(order) = &mut set.order_by {
                lower_order(&mut order.expressions, &[], strict)?;
            }
        }
        Expression::Union(set) => {
            query::cte_values(&mut set.with, target)?;
            if let Some(order) = &mut set.order_by {
                lower_order(&mut order.expressions, &[], strict)?;
            }
        }
        Expression::ArrayAgg(agg) => {
            lower_order(&mut agg.order_by, &[], strict)?;
            if target == Dialect::MYSQL {
                return Ok(Expression::GroupConcat(Box::new(GroupConcatFunc {
                    this: agg.this.clone(),
                    separator: None,
                    order_by: (!agg.order_by.is_empty()).then(|| std::mem::take(&mut agg.order_by)),
                    distinct: agg.distinct,
                    filter: agg.filter.take(),
                    limit: agg.limit.take(),
                    inferred_type: None,
                })));
            }
        }
        Expression::WithinGroup(group) => {
            if let Expression::ListAgg(agg) = &mut group.this {
                lower_order(&mut group.order_by, &[], strict)?;
                return Ok(Expression::GroupConcat(Box::new(GroupConcatFunc {
                    this: agg.this.clone(),
                    separator: Some(agg.separator.take().unwrap_or_else(|| Expression::string(","))),
                    order_by: Some(std::mem::take(&mut group.order_by)),
                    distinct: agg.distinct,
                    filter: agg.filter.take(),
                    limit: None,
                    inferred_type: None,
                })));
            }
        }
        Expression::ApproxDistinct(f) if target == Dialect::STARROCKS => {
            return Ok(Expression::AggregateFunction(Box::new(
                polyglot_sql::expressions::AggregateFunction {
                    source_dialect: None,
                    name: "APPROX_COUNT_DISTINCT".into(),
                    args: vec![f.this.clone()],
                    distinct: f.distinct,
                    filter: f.filter.take(),
                    order_by: std::mem::take(&mut f.order_by),
                    limit: f.limit.take(),
                    ignore_nulls: f.ignore_nulls,
                    inferred_type: None,
                },
            )));
        }
        Expression::ApproxDistinct(_) if target == Dialect::MYSQL && strict => {
            return Err(polyglot_sql::Error::unsupported("APPROX_DISTINCT", "mysql"));
        }
        Expression::AggregateFunction(f) if f.name.eq_ignore_ascii_case("APPROX_PERCENTILE") => {
            f.name = "APPROX_QUANTILE".into();
            if f.args.len() >= 3 {
                f.args[1..].rotate_left(1);
            }
        }
        Expression::AggregateFunction(f) => {
            let normalized = match f.name.to_ascii_uppercase().as_str() {
                "BITWISE_AND_AGG" => "BIT_AND",
                "BITWISE_OR_AGG" => "BIT_OR",
                "BITWISE_XOR_AGG" => "BIT_XOR",
                _ => return Ok(expr),
            };
            f.name = normalized.into();
        }
        _ => {}
    }
    Ok(expr)
}

fn lower_order(keys: &mut Vec<Ordered>, projections: &[Expression], strict: bool) -> polyglot_sql::Result<()> {
    for mut key in std::mem::take(keys) {
        let nulls_first = key.nulls_first.take().unwrap_or(false);
        if nulls_first == key.desc {
            let ordinal = if let Expression::Literal(v) = &key.this {
                if let Literal::Number(n) = v.as_ref() {
                    Some(n.as_str())
                } else {
                    None
                }
            } else {
                None
            };
            let mut rank_input = None;
            if let Some(ordinal) = ordinal {
                rank_input = query::ordinal_key(ordinal, projections).cloned();
                if rank_input.is_none() {
                    if strict {
                        return Err(polyglot_sql::Error::unsupported(
                            "positional NULL ordering",
                            "mysql/starrocks",
                        ));
                    }
                    keys.push(key);
                    continue;
                }
            } else {
                if let Expression::Column(col) = &key.this {
                    if col.table.is_none() {
                        if let Some(alias) = projections.iter().find_map(|p| match p {
                            Expression::Alias(a) if a.alias.name.eq_ignore_ascii_case(&col.name.name) => Some(a),
                            _ => None,
                        }) {
                            key.this = alias.this.clone();
                        }
                    }
                }
            }
            let is_null = Expression::IsNull(Box::new(IsNull {
                this: rank_input.unwrap_or_else(|| key.this.clone()),
                not: false,
                postfix_form: false,
            }));
            let rank = Expression::Case(Box::new(Case {
                operand: None,
                whens: vec![(is_null, Expression::number(i64::from(!nulls_first)))],
                else_: Some(Expression::number(i64::from(nulls_first))),
                comments: vec![],
                inferred_type: None,
            }));
            keys.push(Ordered::asc(rank));
        }
        keys.push(key);
    }
    Ok(())
}

fn call(name: &str, args: Vec<Expression>) -> Expression {
    Expression::Function(Box::new(Function::new(name, args)))
}

fn mysql_array_element(ty: &DataType) -> polyglot_sql::Result<String> {
    let result = match ty {
        DataType::VarChar { length: None, .. } => "TEXT".into(),
        DataType::Array { element_type, .. } => format!("ARRAY<{}>", mysql_array_element(element_type)?),
        _ => polyglot_sql::generate(&Expression::DataType(ty.clone()), Dialect::MYSQL.polyglot())?,
    };
    Ok(result)
}

fn rendered(sql: String) -> Expression {
    Expression::Raw(polyglot_sql::expressions::Raw { sql })
}
