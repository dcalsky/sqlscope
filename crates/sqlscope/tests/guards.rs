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
