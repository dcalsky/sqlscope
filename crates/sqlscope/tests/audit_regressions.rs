use sqlscope::*;

fn pg() -> Options {
    Options::new().dialect(Dialect::POSTGRES)
}

#[test]
fn preceding_ctes_are_not_filtered_again() {
    let sql = "WITH a AS (SELECT id FROM orders), b AS (SELECT id FROM a) SELECT * FROM b";
    let out = apply_row_filter(sql, "tenant = 7", &pg()).unwrap();
    assert_eq!(out.matches("tenant = 7").count(), 1, "{out}");
}

#[test]
fn quoted_cte_does_not_hide_lowercase_physical_table() {
    let sql = "WITH \"Orders\" AS (SELECT id FROM source) SELECT id FROM orders";
    let out = apply_row_filter(sql, "tenant = 7", &pg()).unwrap();
    assert_eq!(out.matches("tenant = 7").count(), 2, "{out}");
    assert_eq!(column_origins(sql, &pg()).unwrap()["orders"], ["id"]);
    assert_eq!(referenced_columns(sql, &pg()).unwrap()["orders"], ["id"]);
}

fn renamed() -> Vec<TableRewrite> {
    vec![TableRewrite::union(
        "s.t",
        UnionRewrite {
            table_alias: "renamed".into(),
            columns: vec!["id".into()],
            branches: vec![TableRef::new("safe")],
        },
    )]
}

#[test]
fn correlated_bare_qualifiers_follow_rewrites() {
    let sql = "SELECT t.id FROM s.t WHERE EXISTS (SELECT 1 FROM u WHERE u.id = t.id)";
    let out = rewrite_tables(sql, &renamed(), &pg()).unwrap();
    assert!(out.contains("u.id = renamed.id"), "{out}");
    let shadow = "SELECT t.id FROM s.t WHERE EXISTS (SELECT 1 FROM u AS t WHERE t.id = 1)";
    let out = rewrite_tables(shadow, &renamed(), &pg()).unwrap();
    assert!(out.contains("WHERE t.id = 1"), "{out}");
}

#[test]
fn partial_column_aliases_preserve_remaining_columns() {
    let opts = pg().schema([("t", ["a", "b"])]);
    for sql in [
        "SELECT * FROM (SELECT a, b FROM t) q(x)",
        "WITH q(x) AS (SELECT a, b FROM t) SELECT * FROM q",
        "WITH q AS (SELECT a, b FROM t) SELECT * FROM q AS r(x)",
        "SELECT * FROM t AS r(x)",
    ] {
        assert_eq!(output_columns(sql, &opts).unwrap().unwrap(), ["x", "b"], "{sql}");
        assert_eq!(column_origins(sql, &opts).unwrap()["t"], ["a", "b"], "{sql}");
    }
}

#[test]
fn union_by_name_preserves_value_attribution() {
    let sql = "SELECT y FROM (SELECT a AS x, b AS y FROM t UNION ALL BY NAME SELECT c AS y, d AS x FROM u) q";
    let origins = column_origins(sql, &Options::new().dialect(Dialect::DUCKDB)).unwrap();
    assert_eq!(origins["t"], ["b"]);
    assert_eq!(origins["u"], ["c"]);
}

#[test]
fn named_window_dependencies_reach_the_output() {
    let sql = "SELECT SUM(a) OVER w FROM t WINDOW w AS (PARTITION BY b ORDER BY c)";
    assert_eq!(column_origins(sql, &pg()).unwrap()["t"], ["a", "b", "c"]);
}

#[test]
fn star_replacement_reads_are_kept_without_schema() {
    let sql = "SELECT * REPLACE (b * 2 AS a) FROM t";
    let opts = Options::new().dialect(Dialect::DUCKDB);
    assert!(referenced_columns(sql, &opts).unwrap()["t"].contains(&"b".to_owned()));
    assert_eq!(column_origins(sql, &opts).unwrap()["t"], ["b"]);
    assert_eq!(
        output_columns("SELECT 1, t.* REPLACE (b * 2 AS a) FROM t", &opts)
            .unwrap()
            .unwrap(),
        ["_col0", "*"]
    );
}

#[test]
fn quoted_aliases_resolve_separately() {
    let sql = "SELECT \"T\".a, t.b FROM first_table AS \"T\" CROSS JOIN second_table AS t";
    let origins = column_origins(sql, &pg()).unwrap();
    assert_eq!(origins["first_table"], ["a"]);
    assert_eq!(origins["second_table"], ["b"]);
}

#[test]
fn duplicate_output_names_keep_positional_lineage() {
    let sql = "SELECT z FROM (SELECT * FROM (SELECT a AS x, b AS x FROM t) q) r(y, z)";
    assert_eq!(column_origins(sql, &pg()).unwrap()["t"], ["b"]);
}

#[test]
fn named_windows_follow_inheritance_and_ignore_unused_definitions() {
    let sql =
        "SELECT SUM(a) OVER w2 FROM t WINDOW w1 AS (PARTITION BY b), w2 AS (w1 ORDER BY c), unused AS (ORDER BY d)";
    assert_eq!(column_origins(sql, &pg()).unwrap()["t"], ["a", "b", "c"]);
}

#[test]
fn injecting_existing_quoted_name_is_rejected() {
    let result = inject_ctes(
        "WITH \"Foo\" AS (SELECT 1) SELECT * FROM \"Foo\"",
        &[CteDef::new("Foo", "SELECT 2")],
        &pg(),
    );
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidArgument);
}

#[test]
fn guards_apply_before_normalization() {
    let sql = format!("SELECT * FROM t {}", "[broadcast] ".repeat(100_000));
    let opts = Options::new().dialect(Dialect::STARROCKS);
    assert_eq!(column_origins(&sql, &opts).unwrap_err().kind(), ErrorKind::Unsupported);
    assert_eq!(
        apply_row_filter(&sql, "a = 1", &opts).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    let predicate = format!("{}a = 1", " ".repeat(1 << 20));
    assert_eq!(
        apply_row_filter("SELECT * FROM t", &predicate, &pg())
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
}

#[test]
fn positional_union_keeps_unnamed_output_positions() {
    let sql = "SELECT x FROM (SELECT 1, a AS x FROM t UNION ALL SELECT b, c FROM u) q ORDER BY x";
    let usages = column_usages(sql, &pg()).unwrap();
    assert!(usages.contains(&ColumnUsage {
        table: "u".into(),
        column: "c".into(),
        clause: Clause::OrderBy
    }));
    assert!(!usages.contains(&ColumnUsage {
        table: "u".into(),
        column: "b".into(),
        clause: Clause::OrderBy
    }));
}

#[test]
fn by_name_union_includes_right_only_columns() {
    let sql = "SELECT y FROM (SELECT a AS x FROM t UNION ALL BY NAME SELECT b AS y FROM u) q";
    let opts = Options::new().dialect(Dialect::DUCKDB);
    let origins = column_origins(sql, &opts).unwrap();
    assert!(origins["t"].is_empty());
    assert_eq!(origins["u"], ["b"]);
    let sql = "SELECT * FROM (SELECT a AS x FROM t UNION ALL BY NAME SELECT b AS y FROM u) q";
    assert_eq!(output_columns(sql, &opts).unwrap().unwrap(), ["x", "y"]);
}

#[test]
fn insert_query_keeps_its_cte_scope() {
    let sql = "WITH c AS (SELECT a FROM src) INSERT INTO dst SELECT * FROM c";
    assert_eq!(
        column_origins(sql, &pg()).unwrap(),
        std::collections::BTreeMap::from([("src".into(), vec!["a".into()])])
    );
    assert_eq!(
        referenced_columns(sql, &pg()).unwrap(),
        std::collections::BTreeMap::from([("src".into(), vec!["a".into()])])
    );
    assert_eq!(output_columns(sql, &pg()).unwrap().unwrap(), ["a"]);
}

#[test]
fn dml_returning_columns_are_reads() {
    for sql in [
        "DELETE FROM t WHERE id = 1 RETURNING payload",
        "UPDATE t SET x = 1 RETURNING payload",
        "INSERT INTO t (x) VALUES (1) RETURNING payload",
    ] {
        assert!(
            referenced_columns(sql, &pg()).unwrap()["t"].contains(&"payload".into()),
            "{sql}"
        );
        assert!(column_usages(sql, &pg()).unwrap().contains(&ColumnUsage {
            table: "t".into(),
            column: "payload".into(),
            clause: Clause::Returning,
        }));
    }
}
