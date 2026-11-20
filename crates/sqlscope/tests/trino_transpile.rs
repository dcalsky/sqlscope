use serde_json::{json, Value};
use sqlscope::{transpile, Dialect, ErrorKind, TranspileMode};

const FIXTURES: &str = include_str!("fixtures/sqlglot/trino.json");

fn actual(input: &str, target: &str, strict: bool) -> Value {
    let mode = if strict {
        TranspileMode::Strict
    } else {
        TranspileMode::BestEffort
    };
    match transpile(input, Dialect::TRINO, target.parse::<Dialect>().unwrap(), mode) {
        Ok(sql) => json!({"sql": sql}),
        Err(e) => json!({"error": match e.kind() {
            ErrorKind::Unsupported => "UnsupportedError",
            ErrorKind::Parse => "ParseError",
            _ => e.kind().as_str(),
        }}),
    }
}

#[test]
fn upstream_target_assertions() {
    let fixtures: Value = serde_json::from_str(FIXTURES).unwrap();
    let assertions = fixtures["explicit"].as_array().unwrap();
    assert_eq!(fixtures["revision"], "0936dbe9192a3b57c6e6992acfcb93d4b429c51f");
    assert_eq!(fixtures["upstream_tests"], 1718);
    assert_eq!(assertions.len(), 4);
    for case in assertions {
        assert_eq!(case["pretty"], false);
        assert_eq!(case["identify"], false);
        let input = case["input"].as_str().unwrap();
        assert_eq!(
            actual(
                input,
                case["target"].as_str().unwrap(),
                case["strict"].as_bool().unwrap()
            ),
            case["expected"],
            "{}\n{input}",
            case["test"]
        );
    }
}

#[test]
fn upstream_trino_inputs_in_both_targets() {
    let fixtures: Value = serde_json::from_str(FIXTURES).unwrap();
    let cases = fixtures["cases"].as_array().unwrap();
    let inputs: std::collections::HashSet<_> = cases.iter().map(|c| c["input"].as_str().unwrap()).collect();
    assert_eq!(inputs.len(), 448);
    assert_eq!(inputs.len(), cases.len());
    let mut differences = Vec::new();
    for case in cases {
        let input = case["input"].as_str().unwrap();
        for target in ["mysql", "starrocks"] {
            for mode in ["default", "strict"] {
                let expected = &case["targets"][target][mode];
                assert!(expected.is_object());
                let result = actual(input, target, mode == "strict");
                if &result != expected {
                    differences.push(format!(
                        "{target} {mode} {input:?}\n expected {expected}\n   actual {result}"
                    ));
                }
            }
        }
    }
    let total = cases.len() * 4;
    assert!(
        differences.is_empty(),
        "{}/{} conversions matched\n{}",
        total - differences.len(),
        total,
        differences.join("\n")
    );
}

#[test]
fn grammar_extensions_compose() {
    let pairs = [
        (
            "SELECT JSON_VALUE(doc, '$.v' DEFAULT bitwise_and(x, 7) ON EMPTY NULL ON ERROR)",
            "SELECT JSON_VALUE(doc, '$.v' DEFAULT x & 7 ON EMPTY NULL ON ERROR)",
        ),
        (
            "SELECT SUM(a) OVER (PARTITION BY EXCLUDING t.c ORDER BY d DESC) FROM t",
            "SELECT SUM(a) OVER (PARTITION BY EXCLUDING t.c ORDER BY d DESC) FROM t",
        ),
        (
            "SELECT 1 UNION SELECT 2 INTERSECT SELECT 3 ORDER BY k",
            "SELECT 1 UNION (SELECT 2 INTERSECT SELECT 3) ORDER BY CASE WHEN k IS NULL THEN 1 ELSE 0 END, k",
        ),
        (
            "TABLE a UNION (TABLE b INTERSECT TABLE c)",
            "SELECT * FROM a UNION (SELECT * FROM b INTERSECT SELECT * FROM c)",
        ),
        (
            "SELECT __SQLSCOPE_TRINO_0(a), JSON_VALUE(doc, '$.v' ERROR ON ERROR)",
            "SELECT __SQLSCOPE_TRINO_0(a), JSON_VALUE(doc, '$.v' ERROR ON ERROR)",
        ),
    ];
    for target in [Dialect::STARROCKS, Dialect::MYSQL] {
        for (input, expected) in pairs {
            assert_eq!(
                transpile(input, Dialect::TRINO, target, TranspileMode::Strict).unwrap(),
                [expected],
                "{input}"
            );
        }
        assert_eq!(
            transpile(
                "SELECT '__é;'; TABLE \"订单\"",
                Dialect::TRINO,
                target,
                TranspileMode::Strict
            )
            .unwrap(),
            ["SELECT '__é;'", "SELECT * FROM `订单`"]
        );
        for invalid in [
            "SELECT (a",
            "SELECT 1)",
            "SELECT JSON_VALUE(doc, '$.v' DEFAULT ON EMPTY)",
        ] {
            assert_eq!(
                transpile(invalid, Dialect::TRINO, target, TranspileMode::Strict)
                    .unwrap_err()
                    .kind(),
                ErrorKind::Parse
            );
        }
    }
}

#[test]
fn source_and_target_are_validated_before_parsing() {
    for (source, target) in [
        (Dialect::MYSQL, Dialect::STARROCKS),
        (Dialect::TRINO, Dialect::POSTGRES),
    ] {
        assert_eq!(
            transpile("SELECT (", source, target, TranspileMode::Strict)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidArgument
        );
    }
}
