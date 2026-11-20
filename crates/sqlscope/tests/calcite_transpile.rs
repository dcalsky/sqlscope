//! Target expectations from Calcite's RelToSqlConverterTest and its SQL fixtures.
use polyglot_sql::{Dialect as Parser, DialectType, TokenType};
use serde_json::Value;
use sqlscope::{transpile, Dialect, ErrorKind, TranspileMode};

fn tokens(sql: &str, dialect: DialectType) -> Vec<(TokenType, String)> {
    Parser::get(dialect)
        .tokenize(sql)
        .unwrap()
        .into_iter()
        .map(|t| (t.token_type, t.text))
        .collect()
}

#[test]
fn calcite_dialect_pairs() {
    let data: Value = serde_json::from_str(include_str!("fixtures/calcite/trino.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 32);
    let mut paired = 0;
    let mut diffs = Vec::new();
    for c in cases {
        let input = c["input"].as_str().unwrap();
        let name = c["test"].as_str().unwrap();
        for (key, target, dialect) in [
            ("mysql", Dialect::MYSQL, DialectType::MySQL),
            ("starrocks", Dialect::STARROCKS, DialectType::StarRocks),
        ] {
            let original = c["calcite"].get(key);
            paired += usize::from(original.is_some());
            let oracle = original.unwrap_or(&c["sqlglot"][key]);
            let reviewed = c["reviewed"].get(key);
            let outcome = transpile(input, Dialect::TRINO, target, TranspileMode::Strict);
            let accepted = match &outcome {
                Ok(sql) if sql.len() == 1 => [original, reviewed.or(Some(oracle))]
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .any(|expected| tokens(&sql[0], dialect) == tokens(expected, dialect)),
                Err(e) => e.kind() == ErrorKind::Unsupported && reviewed.unwrap_or(oracle).is_null(),
                _ => false,
            };
            if !accepted {
                diffs.push(format!(
                    "{name} -> {key}\nexpected {oracle}, reviewed {reviewed:?}\ngot {outcome:?}"
                ));
            }
        }
    }
    assert_eq!(paired, 25);
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}
