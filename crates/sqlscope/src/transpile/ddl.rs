use polyglot_sql::expressions::{AlterTableAction, AlterViewAction};
use polyglot_sql::{traversal::transform_all, Expression};

use super::{lower, rendered, TranspileMode};
use crate::Dialect;

pub(super) fn statement(
    mut expr: Expression,
    target: Dialect,
    mode: TranspileMode,
) -> polyglot_sql::Result<Expression> {
    let sql = |node: &Expression| polyglot_sql::generate(node, target.polyglot());
    let mut suffix = Vec::new();
    match &mut expr {
        Expression::CreateTable(create) if !create.with_properties.is_empty() => {
            let mut properties = Vec::new();
            for (name, source) in std::mem::take(&mut create.with_properties) {
                let parsed = polyglot_sql::parse_one(&source, Dialect::TRINO.polyglot())?;
                let value = transform_all(parsed, &|e| lower(e, target, mode))?;
                let value = sql(&value)?;
                match name.to_ascii_uppercase().as_str() {
                    "LOCATION" => suffix.push(format!("LOCATION {value}")),
                    "PARTITIONED_BY" if target == Dialect::STARROCKS => suffix.push(format!("PARTITION BY {value}")),
                    "PARTITIONED_BY" if mode == TranspileMode::Strict => {
                        return Err(polyglot_sql::Error::unsupported("partitioned table property", "mysql"))
                    }
                    "PARTITIONED_BY" => {}
                    _ if target == Dialect::STARROCKS => {
                        properties.push(format!("{}={value}", sql(&Expression::string(name))?))
                    }
                    _ => suffix.push(format!("{name}={value}")),
                }
            }
            if !properties.is_empty() {
                suffix.push(format!("PROPERTIES ({})", properties.join(", ")));
            }
            if let Some(query) = create.as_select.take() {
                suffix.push(format!("AS {}", sql(&query)?));
            }
        }
        Expression::CreateSchema(schema) if !schema.properties.is_empty() => {
            let mut properties = Vec::new();
            for p in std::mem::take(&mut schema.properties) {
                let Expression::Property(prop) = &p else {
                    suffix.push(sql(&p)?);
                    continue;
                };
                let key = sql(&prop.this)?;
                if let Some(value) = &prop.value {
                    if key.eq_ignore_ascii_case("LOCATION") {
                        suffix.push(format!("LOCATION {}", sql(value)?));
                    } else if target == Dialect::STARROCKS {
                        properties.push(format!("{}={}", sql(&Expression::string(key))?, sql(value)?));
                    } else {
                        suffix.push(sql(&p)?);
                    }
                } else {
                    suffix.push(key);
                }
            }
            if !properties.is_empty() {
                suffix.push(format!("PROPERTIES ({})", properties.join(", ")));
            }
        }
        Expression::Analyze(analyze) => {
            let mut properties = Vec::new();
            for prop in std::mem::take(&mut analyze.properties) {
                if let Expression::Property(p) = &prop {
                    if let (Dialect::STARROCKS, Expression::Identifier(key), Some(value)) =
                        (target, p.this.as_ref(), &p.value)
                    {
                        properties.push(format!("{}={}", sql(&Expression::string(&key.name))?, sql(value)?));
                        continue;
                    }
                }
                suffix.push(sql(&prop)?);
            }
            if !properties.is_empty() {
                suffix.push(format!("PROPERTIES ({})", properties.join(", ")));
            }
        }
        Expression::AlterTable(alter) => {
            if let [AlterTableAction::SetProperty { properties }] = alter.actions.as_slice() {
                let mut assignments = Vec::new();
                for (name, value) in properties {
                    let key = if name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        name.clone()
                    } else {
                        sql(&Expression::string(name))?
                    };
                    assignments.push(format!("{key} = {}", sql(value)?));
                }
                let table = sql(&Expression::Table(Box::new(alter.name.clone())))?;
                let exists = if alter.if_exists { " IF EXISTS" } else { "" };
                return Ok(rendered(format!(
                    "ALTER TABLE{exists} {table} SET PROPERTIES {}",
                    assignments.join(", ")
                )));
            }
        }
        Expression::AlterView(alter) => {
            if let [AlterViewAction::Rename(name)] = alter.actions.as_slice() {
                let table = sql(&Expression::Table(Box::new(alter.name.clone())))?;
                let new_name = sql(&Expression::Table(Box::new(name.clone())))?;
                return Ok(rendered(format!("ALTER VIEW {table} RENAME {new_name}")));
            }
        }
        _ => {}
    }
    if suffix.is_empty() {
        return Ok(expr);
    }
    let mut output = sql(&expr)?;
    for part in suffix {
        output.push(' ');
        output.push_str(&part);
    }
    Ok(rendered(output))
}
