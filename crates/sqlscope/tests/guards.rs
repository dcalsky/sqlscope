#![allow(clippy::type_complexity)]

//! Bounded-input behavior shared by every operation.
mod common;

use sqlscope::*;

#[test]
fn deep_input_is_unsupported_everywhere() {
    let deep = format!("SELECT {}1{} FROM s.t", "(".repeat(4000), ")".repeat(4000));
    let opts = Options::new();
    let results: Vec<(&str, Result<()>)> = vec![
        ("apply_row_filter", apply_row_filter(&deep, "x = 1", &opts).map(drop)),
        (
            "inject_ctes",
            inject_ctes(&deep, &[CteDef::new("bound", "SELECT 1")], &opts).map(drop),
        ),
        (
            "rewrite_tables",
            rewrite_tables(
                &deep,
                &[TableRewrite::inline("s.t", TableRef::new("t").with_schema("safe"))],
                &opts,
            )
            .map(drop),
        ),
        ("column_origins", column_origins(&deep, &opts).map(drop)),
        ("output_columns", output_columns(&deep, &opts).map(drop)),
        ("referenced_columns", referenced_columns(&deep, &opts).map(drop)),
        ("column_usages", column_usages(&deep, &opts).map(drop)),
    ];
    for (name, result) in results {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Unsupported, "{name}");
    }
}

#[test]
fn operations_are_thread_safe() {
    let handles: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                let sql = format!("SELECT a{i}, b FROM t WHERE c > {i}");
                referenced_columns(&sql, &Options::new()).unwrap()["t"].len()
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), 3);
    }
}

#[test]
fn dialect_names() {
    for name in [
        "trino",
        "Postgres",
        "postgresql",
        "pg",
        "mysql",
        "starrocks",
        "spark",
        " hive ",
    ] {
        name.parse::<Dialect>().unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    for name in ["", "nope"] {
        assert_eq!(name.parse::<Dialect>().unwrap_err().kind(), ErrorKind::InvalidArgument);
    }
    assert_eq!(Dialect::default(), Dialect::TRINO);
    assert_eq!("pg".parse::<Dialect>().unwrap().to_string(), "postgresql");
}

/// Nesting well within the documented limits works for every operation (the
/// WebAssembly build must accept the same inputs as native builds).
#[test]
fn moderately_deep_queries_are_accepted() {
    let mut nested = "SELECT a FROM t".to_string();
    for i in 0..120 {
        nested = format!("SELECT a FROM ({nested}) x{i}");
    }
    let parens = format!("SELECT {}a{} AS a FROM t", "(".repeat(200), ")".repeat(200));
    let opts = Options::new();
    for sql in [nested, parens] {
        apply_row_filter(&sql, "x = 1", &opts).unwrap();
        rewrite_tables(
            &sql,
            &[TableRewrite::inline("s.t", TableRef::new("t").with_schema("u"))],
            &opts,
        )
        .unwrap();
        inject_ctes(&sql, &[CteDef::new("c", "SELECT 1")], &opts).unwrap();
        assert_eq!(column_origins(&sql, &opts).unwrap()["t"], ["a"]);
        assert_eq!(output_columns(&sql, &opts).unwrap().unwrap(), ["a"]);
        assert_eq!(referenced_columns(&sql, &opts).unwrap()["t"], ["a"]);
        assert!(!column_usages(&sql, &opts).unwrap().is_empty());
    }
}
