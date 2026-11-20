use std::sync::LazyLock;

use polyglot_sql::expressions::{Cast, DataType};
use polyglot_sql::Expression;
use regex::Regex;

use super::call;

/// Trino infers the precision of a typed literal from the number of fractional
/// second digits, including trailing zeroes. An explicit CAST keeps its type.
pub(super) fn literal(value: &str, timestamp: bool) -> Expression {
    static FRACTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":[0-9]+\.([0-9]+)").unwrap());
    static ZONE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":.*?[a-zA-Z+\-]").unwrap());
    let zoned = ZONE.is_match(value);
    let this = Expression::string(value);
    if timestamp && zoned {
        return call("TIMESTAMP", vec![this]);
    }
    let digits = FRACTION.captures(value).map_or(0, |m| m[1].len() as u32);
    let precision = Some(digits);
    let to = match (timestamp, zoned) {
        (true, _) => DataType::Timestamp {
            precision,
            timezone: false,
        },
        (false, true) => DataType::Custom {
            name: format!("TIMETZ({digits})"),
        },
        (false, false) => DataType::Time {
            precision,
            timezone: false,
        },
    };
    Expression::Cast(Box::new(Cast {
        this,
        to,
        double_colon_syntax: false,
        trailing_comments: vec![],
        format: None,
        default: None,
        inferred_type: None,
    }))
}

/// Map complete Joda fields. If any field lacks a portable spelling, SQLGlot
/// keeps FORMAT_DATETIME and the original pattern intact.
pub(super) fn mysql_format(pattern: &str) -> Option<String> {
    if pattern.is_empty() || pattern.contains(['\'', '%']) {
        return None;
    }
    let mut rest = pattern;
    let mut result = String::new();
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix("ss.SSS") {
            result.push_str("%s.%f_three");
            rest = tail;
            continue;
        }
        let ch = rest.chars().next()?;
        if !ch.is_ascii_alphabetic() {
            result.push(ch);
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        let len = rest.bytes().take_while(|b| *b == ch as u8).count();
        let (field, suffix) = rest.split_at(len);
        rest = suffix;
        result.push_str(match field {
            "yyyy" => "%Y",
            "yy" => "%y",
            "MMMM" => "%M",
            "MMM" => "%b",
            "MM" => "%m",
            "M" => "%c",
            "dd" => "%d",
            "DDD" => "%j",
            "D" => "%-j",
            "EEEE" => "%W",
            "EEE" => "%a",
            "a" => "%p",
            "HH" => "%H",
            "H" => "%k",
            "hh" => "%h",
            "h" => "%l",
            "mm" => "%i",
            "m" => "%-M",
            "ss" => "%s",
            "s" => "%-S",
            _ => return None,
        });
    }
    Some(result.replace("%H:%i:%s", "%T"))
}

pub(super) fn interval_unit(expr: &Expression) -> Option<String> {
    let Expression::Literal(value) = expr else { return None };
    let polyglot_sql::expressions::Literal::String(name) = value.as_ref() else {
        return None;
    };
    let unit = name.to_ascii_uppercase();
    matches!(
        unit.as_str(),
        "YEAR"
            | "QUARTER"
            | "MONTH"
            | "WEEK"
            | "DAY"
            | "HOUR"
            | "MINUTE"
            | "SECOND"
            | "MILLISECOND"
            | "MICROSECOND"
            | "NANOSECOND"
    )
    .then_some(unit)
}

pub(super) fn truncate_mysql(unit: &str, value: &Expression, strict: bool) -> polyglot_sql::Result<Expression> {
    let date = matches!(value, Expression::Cast(c) if c.to == DataType::Date)
        || matches!(value, Expression::Literal(lit) if matches!(lit.as_ref(), polyglot_sql::expressions::Literal::Date(_)));
    let input = polyglot_sql::generate(value, crate::Dialect::MYSQL.polyglot())?;
    let output = if date {
        let (fields, fmt) = match unit {
            "YEAR" => (format!("YEAR({input}), ' 1 1'"), "%Y %c %e"),
            "QUARTER" => (
                format!("YEAR({input}), ' ', QUARTER({input}) * 3 - 2, ' 1'"),
                "%Y %c %e",
            ),
            "MONTH" => (format!("YEAR({input}), ' ', MONTH({input}), ' 1'"), "%Y %c %e"),
            "WEEK" => (format!("YEAR({input}), ' ', WEEK({input}, 1), ' 1'"), "%Y %u %w"),
            "DAY" => return Ok(call("DATE", vec![value.clone()])),
            _ if strict => return Err(polyglot_sql::Error::unsupported("date truncation unit", "mysql")),
            _ => return Ok(call("DATE", vec![value.clone()])),
        };
        format!("STR_TO_DATE(CONCAT({fields}), '{fmt}')")
    } else {
        let origin = "'0000-01-01 00:00:00'";
        format!("DATE_ADD({origin}, INTERVAL (TIMESTAMPDIFF({unit}, {origin}, {input})) {unit})")
    };
    Ok(super::rendered(output))
}
