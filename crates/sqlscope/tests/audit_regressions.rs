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

#[test]
fn subqueries_inside_table_clauses_are_filtered() {
    for (dialect, sql) in [
        (
            Dialect::POSTGRES,
            "SELECT * FROM a TABLESAMPLE BERNOULLI ((SELECT count(*) FROM b))",
        ),
        (
            Dialect::POSTGRES,
            "SELECT * FROM a TABLESAMPLE BERNOULLI (10) REPEATABLE ((SELECT max(s) FROM b))",
        ),
        (
            Dialect::SNOWFLAKE,
            "SELECT * FROM a AT (TIMESTAMP => (SELECT max(ts) FROM b))",
        ),
    ] {
        let out = apply_row_filter(sql, "tenant = 7", &Options::new().dialect(dialect)).unwrap();
        assert_eq!(out.matches("tenant = 7").count(), 2, "{out}");
    }
}

#[test]
fn into_and_lock_targets_are_not_rewritten() {
    for (sql, want) in [
        (
            "SELECT * INTO t2 FROM a",
            "SELECT * INTO t2 FROM (SELECT * FROM a WHERE tenant = 7) AS a",
        ),
        (
            "SELECT * FROM a FOR UPDATE OF a SKIP LOCKED",
            "SELECT * FROM (SELECT * FROM a WHERE tenant = 7) AS a FOR UPDATE OF a SKIP LOCKED",
        ),
        (
            "SELECT * FROM a AS x FOR SHARE OF x",
            "SELECT * FROM (SELECT * FROM a WHERE tenant = 7) AS x FOR SHARE OF x",
        ),
    ] {
        assert_eq!(apply_row_filter(sql, "tenant = 7", &pg()).unwrap(), want);
    }
}

#[test]
fn data_modifying_ctes_are_rejected_by_rewrites() {
    let sql = "WITH d AS (UPDATE b SET x = 1 RETURNING *) SELECT * FROM d";
    let unsupported = |result: Result<String>| result.unwrap_err().kind() == ErrorKind::Unsupported;
    assert!(unsupported(apply_row_filter(sql, "tenant = 7", &pg())));
    let plan = [TableRewrite::inline("s.b", TableRef::new("c").with_schema("s"))];
    assert!(unsupported(rewrite_tables(sql, &plan, &pg())));
    let ctes = [CteDef::new(
        "c",
        "WITH d AS (DELETE FROM b RETURNING *) SELECT * FROM d",
    )];
    assert!(unsupported(inject_ctes("SELECT 1", &ctes, &pg())));
}

#[test]
fn query_ctes_cannot_capture_tables_the_rewrite_reads() {
    let unsupported = |result: Result<String>| result.unwrap_err().kind() == ErrorKind::Unsupported;
    let predicate = "tenant IN (SELECT t FROM allowed)";
    for sql in [
        "WITH allowed AS (SELECT tenant AS t FROM orders) SELECT * FROM orders",
        "SELECT * FROM orders WHERE id IN (WITH allowed AS (SELECT 1 AS t) SELECT id FROM orders)",
    ] {
        assert!(unsupported(apply_row_filter(sql, predicate, &pg())), "{sql}");
    }
    // A CTE defined after the reference, or inside the CTE body that
    // references it, does not capture the predicate's table.
    let out = apply_row_filter("WITH x AS (SELECT * FROM orders) SELECT * FROM x", predicate, &pg()).unwrap();
    assert!(out.contains("FROM allowed"), "{out}");

    let plan = [TableRewrite::inline("s.orders", TableRef::new("orders_v2"))];
    let sql = "WITH orders_v2 AS (SELECT 1) SELECT * FROM s.orders";
    assert!(unsupported(rewrite_tables(sql, &plan, &pg())));
}

#[test]
fn bigquery_correlated_paths_are_not_tables() {
    let bq = Options::new().dialect(Dialect::BIGQUERY);
    let out = apply_row_filter("SELECT * FROM `p.d.a` AS t, t.arr AS e", "tenant = 7", &bq).unwrap();
    assert_eq!(
        out,
        "SELECT * FROM (SELECT * FROM `p`.`d`.`a` WHERE tenant = 7) AS t CROSS JOIN t.arr AS e"
    );
    let out = apply_row_filter("SELECT (SELECT COUNT(*) FROM t.arr) FROM a AS t", "tenant = 7", &bq).unwrap();
    assert_eq!(out.matches("tenant = 7").count(), 1, "{out}");
    // A path is correlated only with earlier relations; a derived table in
    // FROM cannot see its siblings.
    for sql in [
        "SELECT * FROM d.arr, a AS d",
        "SELECT * FROM a AS t, (SELECT * FROM t.arr)",
    ] {
        let out = apply_row_filter(sql, "tenant = 7", &bq).unwrap();
        assert_eq!(out.matches("tenant = 7").count(), 2, "{out}");
    }
    let refs = referenced_columns("SELECT e FROM `p.d.a` AS t, t.arr AS e", &bq).unwrap();
    assert_eq!(refs.keys().collect::<Vec<_>>(), ["p.d.a"]);
    assert!(refs["p.d.a"].contains(&"arr".to_owned()), "{refs:?}");
    let refs = referenced_columns("SELECT * FROM a AS t, (SELECT * FROM t.arr)", &bq).unwrap();
    assert!(refs.contains_key("t.arr"), "{refs:?}");
    // Other dialects read `t.arr` as schema.table.
    let out = apply_row_filter("SELECT * FROM a AS t, t.arr", "tenant = 7", &pg()).unwrap();
    assert_eq!(out.matches("tenant = 7").count(), 2, "{out}");
}

#[test]
fn every_query_position_is_referenced() {
    let opts = pg().schema([("a", ["id", "v"]), ("b", ["id", "w"])]);
    for (dialect, sql) in [
        (Dialect::POSTGRES, "SELECT id FROM a LIMIT (SELECT count(*) FROM b)"),
        (Dialect::POSTGRES, "SELECT id FROM a OFFSET (SELECT count(*) FROM b)"),
        (
            Dialect::POSTGRES,
            "SELECT id FROM a FETCH FIRST (SELECT count(*) FROM b) ROWS ONLY",
        ),
        (
            Dialect::POSTGRES,
            "SELECT DISTINCT ON ((SELECT max(w) FROM b)) id FROM a",
        ),
        (
            Dialect::POSTGRES,
            "SELECT id FROM a UNION SELECT id FROM a LIMIT (SELECT count(*) FROM b)",
        ),
        (
            Dialect::POSTGRES,
            "SELECT id FROM a TABLESAMPLE BERNOULLI ((SELECT count(*) FROM b))",
        ),
        (
            Dialect::POSTGRES,
            "SELECT * FROM (VALUES ((SELECT max(w) FROM b))) AS t(x)",
        ),
        (Dialect::POSTGRES, "WITH unused AS (SELECT w FROM b) SELECT id FROM a"),
        (
            Dialect::POSTGRES,
            "WITH d AS (DELETE FROM b RETURNING *) SELECT id FROM a",
        ),
        (
            Dialect::POSTGRES,
            "INSERT INTO a (id, v) VALUES ((SELECT max(id) FROM b), 1)",
        ),
        (
            Dialect::POSTGRES,
            "INSERT INTO a (id) VALUES (1) ON CONFLICT (id) DO UPDATE SET v = (SELECT max(w) FROM b)",
        ),
        (Dialect::TSQL, "SELECT TOP ((SELECT count(*) FROM b)) id FROM a"),
        (
            Dialect::TSQL,
            "SELECT id FROM a ORDER BY id OFFSET (SELECT count(*) FROM b) ROWS",
        ),
        (
            Dialect::CLICKHOUSE,
            "SELECT id FROM a PREWHERE id IN (SELECT id FROM b)",
        ),
        (
            Dialect::CLICKHOUSE,
            "SELECT id FROM a LIMIT 1 BY (SELECT max(w) FROM b)",
        ),
    ] {
        let refs = referenced_columns(sql, &opts.clone().dialect(dialect)).unwrap();
        assert!(refs.contains_key("b"), "{sql}: {refs:?}");
    }
}

#[test]
fn ctes_resolve_in_their_own_scope() {
    let opts = pg().schema([("b", ["id", "w"])]);
    // An unreferenced CTE still sees its siblings.
    let sql = "WITH x AS (SELECT * FROM b), unused AS (SELECT * FROM x) SELECT 1";
    assert_eq!(
        referenced_columns(sql, &opts).unwrap(),
        [("b".to_owned(), vec!["id".to_owned(), "w".to_owned()])].into()
    );
    let sql = "WITH d AS (UPDATE b SET w = 1 WHERE id = 2 RETURNING id) SELECT * FROM d";
    let usages: Vec<String> = column_usages(sql, &opts)
        .unwrap()
        .into_iter()
        .map(|u| format!("{}.{}@{}", u.table, u.column, u.clause))
        .collect();
    assert_eq!(
        usages,
        ["b.id@RETURNING", "b.id@SELECT", "b.id@WHERE", "b.w@UPDATE_SET_TARGET"]
    );
    let sql = "WITH d AS (DELETE FROM b RETURNING w) SELECT * FROM d";
    assert_eq!(column_origins(sql, &opts).unwrap()["b"], ["w"]);
}

#[test]
fn create_table_like_keeps_written_order() {
    let opts = pg().schema([("src", ["x", "y"])]);
    for (sql, want) in [
        ("CREATE TABLE t (a int, b int, LIKE src)", &["a", "b", "x", "y"][..]),
        ("CREATE TABLE t (LIKE src, a int)", &["x", "y", "a"]),
        ("CREATE TABLE t (a int, LIKE src, b int)", &["a", "x", "y", "b"]),
    ] {
        assert_eq!(output_columns(sql, &opts).unwrap().unwrap(), want, "{sql}");
    }
}

#[test]
fn injected_ctes_cannot_be_captured_by_with_recursive() {
    let sql = "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT * FROM orders, r";
    for (name, query) in [("orders", "SELECT * FROM orders"), ("o2", "SELECT * FROM r")] {
        let ctes = [CteDef::new(name, query)];
        let error = inject_ctes(sql, &ctes, &pg()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{name}");
    }
    let ctes = [
        CteDef::new("a", "SELECT * FROM orders WHERE tenant = 7"),
        CteDef::new("b", "SELECT * FROM a"),
    ];
    let out = inject_ctes(sql, &ctes, &pg()).unwrap();
    assert!(out.starts_with("WITH RECURSIVE a AS"), "{out}");
}

#[test]
fn nested_field_access_is_named_after_the_field() {
    let opts = Options::new().schema([("a", ["id", "v"])]);
    for (sql, want) in [
        ("SELECT a.v.f FROM a", &["f"][..]),
        ("SELECT a.v.f, 1 FROM a", &["f", "_col1"]),
        ("SELECT *, a.v.f FROM a", &["id", "v", "f"]),
        ("SELECT * FROM (SELECT a.v.f FROM a) s", &["f"]),
        ("SELECT a.v.f FROM a UNION ALL SELECT 1", &["f"]),
    ] {
        assert_eq!(output_columns(sql, &opts).unwrap().unwrap(), want, "{sql}");
    }
}

#[test]
fn aliased_merge_relations_resolve_to_their_tables() {
    let opts = pg().schema([("a", ["id", "v"]), ("b", ["id", "w"])]);
    let sql = "MERGE INTO a AS t USING b AS s ON t.id = s.id \
               WHEN MATCHED AND s.w > 0 THEN UPDATE SET v = s.w \
               WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.w)";
    assert_eq!(
        referenced_columns(sql, &opts).unwrap(),
        [
            ("a".to_owned(), vec!["id".to_owned()]),
            ("b".to_owned(), vec!["id".to_owned(), "w".to_owned()])
        ]
        .into()
    );
    assert_eq!(column_origins(sql, &opts).unwrap()["b"], ["id", "w"]);
    let sql = "MERGE INTO a t USING (SELECT * FROM b) s ON t.id = s.id WHEN MATCHED THEN DELETE";
    let refs = referenced_columns(sql, &opts.clone().dialect(Dialect::SNOWFLAKE)).unwrap();
    assert_eq!(refs["a"], ["id"]);
    assert_eq!(refs["b"], ["id", "w"]);
}

#[test]
fn tsql_update_target_alias_is_not_a_table() {
    let opts = Options::new()
        .dialect(Dialect::TSQL)
        .schema([("a", ["id", "v"]), ("b", ["id", "w"])]);
    for sql in [
        "UPDATE x SET x.v = b.w FROM a AS x JOIN b ON x.id = b.id",
        "UPDATE x SET v = b.w FROM a AS x JOIN b ON x.id = b.id",
    ] {
        let refs = referenced_columns(sql, &opts).unwrap();
        assert_eq!(refs.keys().collect::<Vec<_>>(), ["a", "b"], "{sql}");
        assert_eq!(refs["a"], ["id", "v"], "{sql}");
    }
}

#[test]
fn mysql_executable_comments_are_not_ignored() {
    for dialect in [Dialect::MYSQL, Dialect::STARROCKS, Dialect::DORIS] {
        let opts = Options::new().dialect(dialect);
        for sql in [
            "SELECT id FROM a WHERE x = 1 /*! UNION SELECT pw FROM users */",
            "SELECT id FROM a /*!50001 JOIN users ON true */",
            "SELECT id FROM a /*M!, users */",
            "SELECT id FROM a /*!",
        ] {
            let error = referenced_columns(sql, &opts).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{dialect} {sql}");
            assert_eq!(column_usages(sql, &opts).unwrap_err().kind(), ErrorKind::Unsupported);
            assert_eq!(column_origins(sql, &opts).unwrap_err().kind(), ErrorKind::Unsupported);
            assert_eq!(output_columns(sql, &opts).unwrap_err().kind(), ErrorKind::Unsupported);
        }
        // Rewrites regenerate SQL without comments, so hidden code is dropped.
        let out = apply_row_filter("SELECT id FROM a /*!, users */", "t = 1", &opts).unwrap();
        assert!(!out.contains("users"), "{out}");
        // Optimizer hints are not executable comments.
        assert!(referenced_columns("SELECT /*+ SET_VAR(x=1) */ id FROM a", &opts).is_ok());
    }
    // Other dialects read them as ordinary comments.
    assert!(referenced_columns("SELECT id FROM a /*! x */", &pg()).is_ok());
}

#[test]
fn comments_the_engine_does_not_nest_cannot_hide_code() {
    let hidden = [
        "SELECT id /* /* */ , (SELECT pw FROM users) -- */\nFROM a",
        "SELECT id FROM a\n/* /* */ UNION SELECT pw FROM users -- */",
    ];
    for dialect in [Dialect::MYSQL, Dialect::BIGQUERY, Dialect::HIVE] {
        let opts = Options::new().dialect(dialect);
        for sql in hidden {
            let error = referenced_columns(sql, &opts).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{dialect} {sql}");
        }
        assert!(referenced_columns("SELECT /*+ X */ id /* a */ FROM a /* b */", &opts).is_ok());
    }
    // Engines that nest comments agree with the parser.
    for dialect in [Dialect::POSTGRES, Dialect::SPARK] {
        let refs = referenced_columns(hidden[0], &Options::new().dialect(dialect)).unwrap();
        assert_eq!(refs.keys().collect::<Vec<_>>(), ["a"]);
    }
}

#[test]
fn tables_named_by_expressions_are_rejected() {
    for (dialect, sql) in [
        (Dialect::SNOWFLAKE, "SELECT * FROM IDENTIFIER('orders')"),
        (Dialect::SNOWFLAKE, "SELECT * FROM t JOIN IDENTIFIER($tbl) AS x ON true"),
        (Dialect::SNOWFLAKE, "SELECT IDENTIFIER('secret') FROM t"),
        (Dialect::DATABRICKS, "SELECT * FROM IDENTIFIER('orders')"),
        (Dialect::SNOWFLAKE, "SELECT * FROM TABLE('orders')"),
        (Dialect::SNOWFLAKE, "SELECT * FROM TABLE(orders)"),
        (Dialect::CLICKHOUSE, "SELECT * FROM remote('localhost', default.orders)"),
        (Dialect::CLICKHOUSE, "SELECT * FROM cluster('c', default, orders)"),
        (
            Dialect::CLICKHOUSE,
            "SELECT * FROM t JOIN merge(default, '^ord') AS m ON true",
        ),
        (Dialect::DUCKDB, "SELECT * FROM query_table('orders')"),
        (Dialect::DUCKDB, "SELECT * FROM query('SELECT * FROM orders')"),
        (
            Dialect::TRINO,
            "SELECT * FROM TABLE(system.query(query => 'SELECT * FROM orders'))",
        ),
    ] {
        let opts = Options::new().dialect(dialect);
        let error = apply_row_filter(sql, "tenant = 7", &opts).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{dialect} {sql}");
        let error = referenced_columns(sql, &opts).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{dialect} {sql}");
    }
    // Table functions that read no table remain supported.
    for (dialect, sql) in [
        (
            Dialect::SNOWFLAKE,
            "SELECT * FROM TABLE(FLATTEN(input => ARRAY_CONSTRUCT(1)))",
        ),
        (Dialect::POSTGRES, "SELECT * FROM generate_series(1, 3)"),
        (Dialect::SPARK, "SELECT * FROM range(10)"),
    ] {
        let opts = Options::new().dialect(dialect);
        assert_eq!(apply_row_filter(sql, "tenant = 7", &opts).unwrap(), sql, "{dialect}");
    }
}

#[test]
fn bigquery_table_arguments_are_filtered() {
    let bq = Options::new().dialect(Dialect::BIGQUERY);
    let sql = "SELECT * FROM ML.PREDICT(MODEL m, TABLE secret)";
    assert_eq!(
        apply_row_filter(sql, "tenant = 7", &bq).unwrap(),
        "SELECT * FROM ML.PREDICT(MODEL m, (SELECT * FROM secret WHERE tenant = 7))"
    );
    assert_eq!(
        referenced_columns(sql, &bq).unwrap(),
        [("secret".to_owned(), Vec::new())].into()
    );
    let out = apply_row_filter(
        "SELECT * FROM ML.PREDICT(MODEL m, (SELECT * FROM secret))",
        "tenant = 7",
        &bq,
    )
    .unwrap();
    assert_eq!(out.matches("tenant = 7").count(), 1, "{out}");
}

#[test]
fn clickhouse_table_reads_in_expressions_are_rejected() {
    let ch = Options::new().dialect(Dialect::CLICKHOUSE);
    for sql in [
        "SELECT * FROM a WHERE id IN b",
        "SELECT * FROM a WHERE id NOT IN db.b",
        "SELECT joinGet('db.j', 'v', id) FROM a",
    ] {
        assert_eq!(
            apply_row_filter(sql, "tenant = 7", &ch).unwrap_err().kind(),
            ErrorKind::Unsupported,
            "{sql}"
        );
        assert_eq!(
            referenced_columns(sql, &ch).unwrap_err().kind(),
            ErrorKind::Unsupported,
            "{sql}"
        );
    }
    let out = apply_row_filter("SELECT * FROM a WHERE id IN (SELECT id FROM b)", "tenant = 7", &ch).unwrap();
    assert_eq!(out.matches("tenant = 7").count(), 2, "{out}");
    assert!(apply_row_filter("SELECT * FROM a WHERE id IN (1, 2)", "tenant = 7", &ch).is_ok());
}

#[test]
fn match_recognize_input_is_filtered_and_resolved() {
    let sf = Options::new().dialect(Dialect::SNOWFLAKE);
    let sql = "SELECT * FROM a MATCH_RECOGNIZE (PARTITION BY k ORDER BY t MEASURES FIRST(v) AS f PATTERN (x+) DEFINE x AS v > 0)";
    assert_eq!(
        apply_row_filter(sql, "tenant = 7", &sf).unwrap(),
        "SELECT * FROM (SELECT * FROM a WHERE tenant = 7) MATCH_RECOGNIZE (PARTITION BY k ORDER BY t \
         MEASURES FIRST(v) AS f PATTERN (x+) DEFINE x AS v > 0)"
    );
    let refs = referenced_columns(sql, &sf.clone().schema([("a", ["t", "v", "k"])])).unwrap();
    assert_eq!(refs["a"], ["k", "t", "v"]);
}

#[test]
fn oracle_database_links_alias_the_bare_name() {
    let ora = Options::new().dialect(Dialect::ORACLE);
    assert_eq!(
        apply_row_filter("SELECT a.x FROM a@remote", "tenant = 7", &ora).unwrap(),
        "SELECT a.x FROM (SELECT * FROM a@remote WHERE tenant = 7) a"
    );
}

#[test]
fn keyword_named_tables_get_quoted_aliases() {
    for (dialect, sql, want) in [
        (
            Dialect::ORACLE,
            "SELECT * FROM hive.raw.use",
            "SELECT * FROM (SELECT * FROM hive.raw.use WHERE t = 1) \"use\"",
        ),
        (
            Dialect::TRINO,
            "SELECT * FROM s.window",
            "SELECT * FROM (SELECT * FROM s.\"window\" WHERE t = 1) AS \"window\"",
        ),
        (
            Dialect::POSTGRES,
            "SELECT * FROM s.orders",
            "SELECT * FROM (SELECT * FROM s.orders WHERE t = 1) AS orders",
        ),
    ] {
        assert_eq!(
            apply_row_filter(sql, "t = 1", &Options::new().dialect(dialect)).unwrap(),
            want
        );
    }
}

#[test]
fn engine_rejections_and_parser_panics_are_errors() {
    let bq = Options::new().dialect(Dialect::BIGQUERY);
    let error = output_columns("SELECT 1 AS a UNION ALL BY NAME SELECT 2 AS b", &bq).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    // The parser panics on unclosed parentheses after T-SQL IF; they are
    // rejected before parsing, so no build (WebAssembly included) aborts.
    let tsql = Options::new().dialect(Dialect::TSQL);
    for sql in ["SELECT IF(IF(", "SELECT IF(", "SELECT IF(x, (1"] {
        assert_eq!(
            referenced_columns(sql, &tsql).unwrap_err().kind(),
            ErrorKind::Parse,
            "{sql}"
        );
        assert_eq!(
            apply_row_filter(sql, "t = 1", &tsql).unwrap_err().kind(),
            ErrorKind::Parse,
            "{sql}"
        );
    }
    assert!(referenced_columns("SELECT IIF(x > 1, 1, 0) FROM t", &tsql).is_ok());
    // The parser reads a column as the source of this INSERT.
    assert_eq!(
        output_columns("INSERT INTO t DEFAULT V", &Options::new()).unwrap(),
        None
    );
    // Input whose own regeneration does not parse is unsupported, not a bug.
    let ora = Options::new().dialect(Dialect::ORACLE);
    let error = apply_row_filter("SELECT (SELECT id FROM t) AS in", "x = 1", &ora).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
}

#[test]
fn starrocks_subscripts_named_like_join_hints_are_kept() {
    let sr = Options::new().dialect(Dialect::STARROCKS);
    assert_eq!(
        apply_row_filter("SELECT arr[shuffle] FROM t JOIN [broadcast] u ON true", "x = 1", &sr).unwrap(),
        "SELECT arr[shuffle] FROM (SELECT * FROM t WHERE x = 1) AS t JOIN (SELECT * FROM u WHERE x = 1) AS u ON TRUE"
    );
    assert_eq!(
        referenced_columns("SELECT arr[shuffle] FROM t", &sr).unwrap()["t"],
        ["arr", "shuffle"]
    );
}

#[test]
fn constructs_the_generator_drops_are_not_rewritten() {
    for (dialect, sql) in [
        (Dialect::SNOWFLAKE, "SELECT * FROM a, b WHERE a.id = b.id(+)"),
        (
            Dialect::ORACLE,
            "SELECT NTH_VALUE(x, 2) FROM LAST OVER (ORDER BY y) FROM a",
        ),
        (Dialect::BIGQUERY, "SELECT LAST_DAY(d, WEEK(MONDAY)) FROM a"),
        (Dialect::BIGQUERY, "SELECT CAST(x AS NUMERIC(10, 2)) FROM a"),
        (Dialect::BIGQUERY, "SELECT SAFE_CAST(x AS ARRAY<NUMERIC(10, 2)>) FROM a"),
    ] {
        let options = Options::new().dialect(dialect);
        let error = apply_row_filter(sql, "t = 1", &options).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{sql}: {error}");
        assert!(
            inject_ctes(sql, &[CteDef::new("c", "SELECT 1")], &options).is_err(),
            "{sql}"
        );
        assert!(referenced_columns(sql, &options).is_ok(), "{sql}");
    }
    for (dialect, sql) in [
        (Dialect::ORACLE, "SELECT * FROM a, b WHERE a.id = b.id(+)"),
        (
            Dialect::SNOWFLAKE,
            "SELECT NTH_VALUE(x, 2) FROM LAST OVER (ORDER BY y) FROM a",
        ),
        (
            Dialect::BIGQUERY,
            "SELECT LAST_DAY(d, WEEK(SUNDAY)), CAST(x AS NUMERIC) FROM a",
        ),
        (Dialect::POSTGRES, "SELECT CAST(x AS NUMERIC(10, 2)) FROM a"),
    ] {
        let out = apply_row_filter(sql, "t = 1", &Options::new().dialect(dialect)).unwrap();
        assert!(out.contains("t = 1"), "{out}");
    }
}

#[test]
fn clickhouse_input_the_parser_skips_is_rejected() {
    let ch = Options::new().dialect(Dialect::CLICKHOUSE);
    for sql in [
        "SELECT * FROM a LOCAL JOIN secret USING id",
        "SELECT * FROM a{i}, secret",
        "SELECT * FROM a AS OF SCN 5 secret",
    ] {
        let error = apply_row_filter(sql, "t = 1", &ch).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{sql}: {error}");
    }
    for sql in [
        "SELECT k, count() FROM a GROUP BY k WITH TOTALS",
        "SELECT * FROM a SETTINGS max_threads = 1",
        "SELECT * FROM a FORMAT JSONEachRow",
        "SELECT * FROM a WHERE id = {p: UInt32}",
        "SELECT now() - INTERVAL 1 DAY FROM a",
        "SELECT j.a.b FROM a",
    ] {
        let out = apply_row_filter(sql, "t = 1", &ch).unwrap();
        assert!(out.contains("t = 1"), "{out}");
    }
}

#[test]
fn keyword_aliases_the_engine_reads_as_syntax_are_rejected() {
    for (dialect, sql) in [
        (Dialect::CLICKHOUSE, "SELECT * FROM a GLOBAL JOIN b USING id"),
        (Dialect::ORACLE, "SELECT * FROM a PARTITION (p1)"),
        (Dialect::DORIS, "SELECT * FROM a PARTITIONS (p1)"),
    ] {
        let error = apply_row_filter(sql, "t = 1", &Options::new().dialect(dialect)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{sql}: {error}");
    }
}

#[test]
fn describe_in_from_is_not_an_internal_error() {
    let duck = Options::new().dialect(Dialect::DUCKDB);
    for result in [
        apply_row_filter("SELECT * FROM (DESCRIBE t)", "x = 1", &duck).map(|_| ()),
        referenced_columns("SELECT * FROM (DESCRIBE t)", &duck).map(|_| ()),
        output_columns("SELECT * FROM (DESCRIBE t)", &duck).map(|_| ()),
    ] {
        if let Err(error) = result {
            assert_ne!(error.kind(), ErrorKind::Internal, "{error}");
        }
    }
}

#[test]
fn every_select_into_target_is_left_alone() {
    for dialect in [Dialect::ORACLE, Dialect::TSQL] {
        let options = Options::new().dialect(dialect);
        let sql = "SELECT a, b INTO v1, v2 FROM d";
        let out = apply_row_filter(sql, "t = 1", &options).unwrap();
        assert!(
            out.starts_with("SELECT a, b INTO v1, v2 FROM (SELECT * FROM d WHERE t = 1)"),
            "{out}"
        );
        let refs = referenced_columns(sql, &options).unwrap();
        assert_eq!(refs.keys().collect::<Vec<_>>(), ["d"]);
    }
}

#[test]
fn inputs_the_generator_cannot_reproduce_are_unsupported() {
    let ch = Options::new().dialect(Dialect::CLICKHOUSE);
    for sql in [
        "SELECT * FROM a TABLESAMPLE BERNOULLI (10)",
        "SELECT * FROM a TABLESAMPLE SYSTEM (10 PERCENT)",
    ] {
        let error = apply_row_filter(sql, "t = 1", &ch).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{sql}: {error}");
    }
    assert!(apply_row_filter("SELECT * FROM a SAMPLE 0.1", "t = 1", &ch).is_ok());
    let sf = Options::new().dialect(Dialect::SNOWFLAKE);
    let chained = "SELECT * FROM t UNPIVOT(a FOR b IN (c, d)) UNPIVOT(e FOR f IN (g, h))";
    let error = apply_row_filter(chained, "x = 1", &sf).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    assert!(apply_row_filter("SELECT * FROM t UNPIVOT(a FOR b IN (c, d))", "x = 1", &sf).is_ok());
}

#[test]
fn alias_wrapped_join_tables_keep_their_alias() {
    let sf = Options::new().dialect(Dialect::SNOWFLAKE);
    assert_eq!(
        apply_row_filter(
            "SELECT * FROM x ASOF JOIN y LIMIT MATCH_CONDITION (x.a > LIMIT.a)",
            "t = 1",
            &sf
        )
        .unwrap(),
        "SELECT * FROM (SELECT * FROM x WHERE t = 1) AS x ASOF JOIN (SELECT * FROM y WHERE t = 1) AS LIMIT \
         MATCH_CONDITION (x.a > LIMIT.a)"
    );
}

#[test]
fn tsql_table_hints_without_with_are_rejected() {
    let tsql = Options::new().dialect(Dialect::TSQL);
    for sql in [
        "SELECT * FROM dbo.t (NOLOCK)",
        "SELECT * FROM [dbo].[t] (NOLOCK) x",
        "SELECT * FROM a JOIN t (NOLOCK, INDEX(i)) ON a.id = t.id",
        "SELECT * FROM a CROSS APPLY t (NOLOCK)",
        "SELECT * FROM t AS x (NOLOCK)",
        "UPDATE s SET x = 1 FROM dbo.t AS s (ROWLOCK)",
    ] {
        assert_eq!(
            referenced_columns(sql, &tsql).unwrap_err().kind(),
            ErrorKind::Unsupported,
            "{sql}"
        );
        assert_eq!(
            apply_row_filter(sql, "t = 1", &tsql).unwrap_err().kind(),
            ErrorKind::Unsupported,
            "{sql}"
        );
    }
    assert_eq!(
        apply_row_filter("SELECT * FROM dbo.t WITH (NOLOCK)", "x = 1", &tsql).unwrap(),
        "SELECT * FROM (SELECT * FROM dbo.t WITH (NOLOCK) WHERE x = 1) AS t"
    );
    assert!(apply_row_filter("SELECT ISNULL(nolock, 0) FROM dbo.fn(1) AS f", "x = 1", &tsql).is_ok());
}

#[test]
fn redshift_partiql_paths_are_not_tables() {
    let rs = Options::new().dialect("redshift".parse().unwrap());
    let sql = "SELECT c.name, o.k FROM customers AS c, c.orders AS o";
    assert_eq!(
        apply_row_filter(sql, "t = 1", &rs).unwrap(),
        "SELECT c.name, o.k FROM (SELECT * FROM customers WHERE t = 1) AS c, c.orders AS o"
    );
    assert_eq!(
        referenced_columns(sql, &rs).unwrap()["customers"],
        ["k", "name", "orders"]
    );
    let indexed = "SELECT c.name FROM customers AS c, c.orders AS o AT i";
    assert_eq!(
        referenced_columns(indexed, &rs).unwrap()["customers"],
        ["name", "orders"]
    );
    // Elsewhere the same text names a table in schema `c`.
    let pg = referenced_columns(sql, &pg()).unwrap();
    assert!(pg.contains_key("c.orders"), "{pg:?}");
}

#[test]
fn dual_is_exempt_only_where_it_is_the_dummy_table() {
    for dialect in [Dialect::ORACLE, Dialect::MYSQL, Dialect::SNOWFLAKE] {
        let sql = "SELECT 1 FROM dual";
        assert_eq!(
            apply_row_filter(sql, "t = 1", &Options::new().dialect(dialect)).unwrap(),
            sql
        );
    }
    for (dialect, sql) in [
        (Dialect::POSTGRES, "SELECT * FROM dual"),
        (Dialect::TRINO, "SELECT * FROM dual"),
        (Dialect::TSQL, "SELECT * FROM dual"),
        (Dialect::ORACLE, "SELECT * FROM app.dual"),
    ] {
        let out = apply_row_filter(sql, "t = 1", &Options::new().dialect(dialect)).unwrap();
        assert!(out.contains("WHERE t = 1"), "{out}");
    }
}

#[test]
fn parenthesized_joins_resolve_their_aliases() {
    let refs = referenced_columns("SELECT x.v FROM (a AS x JOIN b ON x.id = b.id)", &pg()).unwrap();
    assert_eq!(refs.keys().collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(refs["a"], ["id", "v"]);
    let usages = column_usages("SELECT 1 FROM (a JOIN b USING (k))", &pg()).unwrap();
    assert!(
        usages.iter().all(|usage| usage.clause == Clause::JoinUsing),
        "{usages:?}"
    );
    let values = "SELECT 1 FROM ((VALUES (1)) AS v(id) LEFT JOIN t ON v.id = t.id)";
    assert_eq!(
        referenced_columns(values, &pg()).unwrap().keys().collect::<Vec<_>>(),
        ["t"]
    );
}

#[test]
fn paths_through_unnest_and_path_aliases_are_not_tables() {
    let bq = Options::new().dialect(Dialect::BIGQUERY);
    for sql in [
        "SELECT l.x FROM t AS c, UNNEST(c.arr) AS o, o.items AS l",
        "SELECT l.x FROM t AS c, c.arr AS o, o.items AS l",
    ] {
        let refs = referenced_columns(sql, &bq).unwrap();
        assert_eq!(refs.keys().collect::<Vec<_>>(), ["t"], "{sql}");
        let out = apply_row_filter(sql, "k = 1", &bq).unwrap();
        assert_eq!(out.matches("k = 1").count(), 1, "{out}");
    }
}

#[test]
fn bigquery_dashed_two_part_paths_match_scopes_and_rewrites() {
    let bq = Options::new().dialect(Dialect::BIGQUERY);
    let sql = "SELECT * FROM x-0.orders";
    let out = apply_row_filter(sql, "t = 1", &bq.clone().table_names(["orders"])).unwrap();
    assert!(out.contains("WHERE t = 1"), "{out}");
    let out = rewrite_tables(
        sql,
        &[TableRewrite::inline("x-0.orders", TableRef::new("v2").with_schema("s"))],
        &bq,
    )
    .unwrap();
    assert_eq!(out, "SELECT * FROM (SELECT * FROM s.v2) AS orders");
}

#[test]
fn tsql_default_schema_paths_keep_their_empty_schema() {
    let tsql = Options::new().dialect(Dialect::TSQL);
    let refs = referenced_columns("SELECT b.x FROM a..b", &tsql).unwrap();
    assert_eq!(refs.keys().collect::<Vec<_>>(), ["a..b"]);
}

#[test]
fn predicates_the_parser_would_truncate_are_rejected() {
    let ch = Options::new().dialect(Dialect::CLICKHOUSE);
    for predicate in [
        "tenant = 7) AND (deleted = 0",
        "tenant = 7)",
        "tenant = 7 WHERE deleted = 0",
    ] {
        assert!(
            apply_row_filter("SELECT * FROM a", predicate, &ch).is_err(),
            "{predicate}"
        );
    }
    for dialect in [Dialect::TRINO, Dialect::TSQL] {
        let options = Options::new().dialect(dialect);
        assert!(apply_row_filter("SELECT * FROM a", "tenant = 7 /*", &options).is_err());
    }
    assert!(apply_row_filter(
        "SELECT * FROM a",
        "t = 1 FOR UPDATE",
        &Options::new().dialect(Dialect::TSQL)
    )
    .is_err());
    assert_eq!(
        apply_row_filter("SELECT * FROM a", "(t = 1) OR (u = 2)", &ch).unwrap(),
        "SELECT * FROM (SELECT * FROM a WHERE (t = 1) OR (u = 2)) AS a"
    );
    // A stray closing parenthesis in the query is a parse error too.
    let error = referenced_columns("SELECT * FROM a) JOIN secret USING (id", &ch).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Parse);
}

#[test]
fn system_time_clauses_cannot_swallow_the_query() {
    for (dialect, sql) in [
        (
            Dialect::TSQL,
            "SELECT x FROM a.b FOR SYSTEM_TIME FROM c TO d UNION ALL SELECT x FROM s.secret",
        ),
        (
            Dialect::TSQL,
            "SELECT x FROM a.b FOR SYSTEM_TIME FROM c TO d JOIN s.secret ON 1 = 1",
        ),
        (
            Dialect::TRINO,
            "SELECT * FROM a FOR SYSTEM_TIME FROM '2020' TO '2021' UNION ALL SELECT * FROM secret",
        ),
    ] {
        let options = Options::new().dialect(dialect);
        let error = apply_row_filter(sql, "t = 1", &options).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{sql}: {error}");
        assert_eq!(
            referenced_columns(sql, &options).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }
    let tsql = Options::new().dialect(Dialect::TSQL);
    for sql in [
        "SELECT x FROM a.b FOR SYSTEM_TIME FROM '2020-01-01' TO @d",
        "SELECT x FROM a.b FOR SYSTEM_TIME BETWEEN SYSUTCDATETIME() AND @d",
        "SELECT x FROM a.b FOR SYSTEM_TIME CONTAINED IN ('a', 'b')",
        "SELECT x FROM a.b FOR SYSTEM_TIME ALL",
    ] {
        let out = apply_row_filter(sql, "t = 1", &tsql).unwrap();
        assert!(out.contains("WHERE t = 1"), "{out}");
    }
}
