//! Normalize Trino function signatures before rendering in the target dialect.
use polyglot_sql::expressions::{BinaryOp, Function, Literal, UnaryOp};
use polyglot_sql::Expression;

use super::{call, rendered, time};
use crate::Dialect;

pub(super) fn lower(mut f: Box<Function>, target: Dialect, strict: bool) -> polyglot_sql::Result<Expression> {
    let name = f.name.to_ascii_uppercase();
    let args = &mut f.args;
    let renamed = match (name.as_str(), args.as_mut_slice()) {
        ("FORMAT_DATETIME", [_, Expression::Literal(format)]) => {
            let Literal::String(value) = format.as_mut() else {
                return Ok(Expression::Function(f));
            };
            let Some(pattern) = time::mysql_format(value) else {
                return Ok(Expression::Function(f));
            };
            *value = pattern;
            "DATE_FORMAT"
        }
        ("DATE_ADD", [unit, amount, date]) if time::interval_unit(unit).is_some() => {
            let unit = time::interval_unit(unit).unwrap();
            let mut sql = polyglot_sql::generate(amount, target.polyglot())?;
            if !matches!(
                amount,
                Expression::Literal(_) | Expression::Column(_) | Expression::Neg(_) | Expression::Paren(_)
            ) {
                sql = format!("({sql})");
            }
            return Ok(call(
                "DATE_ADD",
                vec![date.clone(), rendered(format!("INTERVAL {sql} {unit}"))],
            ));
        }
        ("DATE_TRUNC", [unit, value]) if time::interval_unit(unit).is_some() => {
            let name = time::interval_unit(unit).unwrap();
            if target == Dialect::MYSQL {
                return time::truncate_mysql(&name, value, strict);
            }
            *unit = Expression::string(name);
            "DATE_TRUNC"
        }
        ("MAP", [Expression::ArrayFunc(keys), Expression::ArrayFunc(values)]) if target == Dialect::STARROCKS => {
            let args = keys
                .expressions
                .iter()
                .zip(&values.expressions)
                .flat_map(|(k, v)| [k.clone(), v.clone()])
                .collect();
            return Ok(call("MAP", args));
        }
        ("MAP", _) if target == Dialect::STARROCKS && strict => {
            return Err(polyglot_sql::Error::unsupported(
                "MAP with non-literal arrays",
                "starrocks",
            ));
        }
        ("DATE_DIFF", [Expression::Literal(unit), start, end]) => {
            if let Literal::String(text) = unit.as_mut() {
                text.make_ascii_uppercase();
                if target == Dialect::MYSQL {
                    return Ok(call("DATEDIFF", vec![end.clone(), start.clone()]));
                }
                std::mem::swap(start, end);
            }
            "DATE_DIFF"
        }
        ("APPROX_DISTINCT", _) if target == Dialect::MYSQL && strict => {
            return Err(polyglot_sql::Error::unsupported("APPROX_DISTINCT", "mysql"));
        }
        ("APPROX_DISTINCT", _) if target == Dialect::STARROCKS => {
            if strict && args.len() > 1 {
                return Err(polyglot_sql::Error::unsupported(
                    "APPROX_DISTINCT accuracy",
                    "starrocks",
                ));
            }
            args.truncate(1);
            "APPROX_COUNT_DISTINCT"
        }
        ("BITWISE_AND" | "BITWISE_OR" | "BITWISE_XOR", [left, right]) => {
            let op = Box::new(BinaryOp::new(left.clone(), right.clone()));
            return Ok(match name.as_str() {
                "BITWISE_AND" => Expression::BitwiseAnd(op),
                "BITWISE_OR" => Expression::BitwiseOr(op),
                _ => Expression::BitwiseXor(op),
            });
        }
        ("BITWISE_NOT", [operand]) => return Ok(Expression::BitwiseNot(Box::new(UnaryOp::new(operand.clone())))),
        ("CURRENT_USER", []) => {
            f.no_parens = false;
            "CURRENT_USER"
        }
        ("DOW", [_]) => "DAYOFWEEK_ISO",
        ("DOY", [_]) => "DAYOFYEAR",
        ("LEVENSHTEIN_DISTANCE", [_, _]) => "LEVENSHTEIN",
        ("TO_HEX", [_]) => "HEX",
        ("FROM_HEX", [_]) => "UNHEX",
        ("MD5", [_]) => "MD5_DIGEST",
        ("SHA1", [_]) => "S_H_A1_DIGEST",
        ("SHA256" | "SHA512", [_]) => {
            args.push(Expression::number(if name == "SHA256" { 256 } else { 512 }));
            "S_H_A2_DIGEST"
        }
        ("SLICE", [_, _, _]) => "ARRAY_SLICE",
        ("LOG2" | "LOG10", [_]) => {
            args.insert(0, Expression::number(if name == "LOG2" { 2 } else { 10 }));
            "LOG"
        }
        ("SEQUENCE", _) => "GENERATE_SERIES",
        ("VERSION", []) if target == Dialect::STARROCKS => "CURRENT_VERSION",
        ("FROM_ISO8601_TIMESTAMP" | "FROM_ISO8601_TIMESTAMP_NANOS", [_]) => "TIMESTAMP",
        ("REGEXP_EXTRACT" | "REGEXP_EXTRACT_ALL", [_, _]) => {
            args.push(Expression::number(0));
            &name
        }
        _ => return Ok(Expression::Function(f)),
    };
    f.name = renamed.to_owned();
    Ok(Expression::Function(f))
}
