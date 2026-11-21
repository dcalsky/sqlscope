use sqlscope::{output_lineage, Dialect, ErrorKind, Options, OutputColumnLineage};

fn col(name: &str, sources: &[(&str, &[&str])]) -> OutputColumnLineage {
    OutputColumnLineage {
        name: name.into(),
        sources: sources
            .iter()
            .map(|(table, cols)| (table.to_string(), cols.iter().map(|c| c.to_string()).collect()))
            .collect(),
    }
}

#[test]
fn follows_ctes_by_position_excluding_filters() {
    let sql = "WITH v AS (SELECT p, q FROM orders WHERE enabled) SELECT p AS x, p*p+q AS x, 1 FROM v";
    assert_eq!(
        output_lineage(sql, &Options::new()).unwrap().unwrap(),
        [
            col("x", &[("orders", &["p"])]),
            col("x", &[("orders", &["p", "q"])]),
            col("_col2", &[])
        ]
    );
}

#[test]
fn wildcard_and_declared_names() {
    let opts = Options::new().schema([("t", ["b", "a"])]);
    for sql in [
        "CREATE VIEW v (x,y) AS SELECT * FROM t",
        "INSERT INTO v (x,y) SELECT * FROM t",
    ] {
        assert_eq!(
            output_lineage(sql, &opts).unwrap().unwrap(),
            [col("x", &[("t", &["b"])]), col("y", &[("t", &["a"])])]
        );
    }
    assert_eq!(
        output_lineage("SELECT * FROM t", &Options::new()).unwrap().unwrap(),
        [col("*", &[("t", &["*"])])]
    );
    assert_eq!(output_lineage("DROP TABLE t", &opts).unwrap(), None);
}

#[test]
fn sets_align_by_position_or_name() {
    let opts = Options::new().dialect(Dialect::DUCKDB);
    let values = |sql: &str| output_lineage(sql, &opts).unwrap().unwrap();
    assert_eq!(
        values("SELECT a AS x, b AS y FROM t UNION ALL BY NAME SELECT p AS y, q AS x FROM u"),
        [
            col("x", &[("t", &["a"]), ("u", &["q"])]),
            col("y", &[("t", &["b"]), ("u", &["p"])])
        ]
    );
    for op in ["EXCEPT", "INTERSECT"] {
        assert_eq!(
            values(&format!("SELECT a FROM t {op} SELECT b FROM u")),
            [col("a", &[("t", &["a"])])]
        );
    }
    assert_eq!(
        values(r#"SELECT a AS "*" FROM t UNION ALL SELECT b FROM u"#),
        [col("*", &[("t", &["a"]), ("u", &["b"])])]
    );
}

#[test]
fn unresolved_positions_report_errors() {
    for sql in [
        "SELECT a,b FROM t UNION ALL SELECT * FROM u",
        "CREATE VIEW v (x,y) AS SELECT a FROM t",
        "SELECT * FROM (SELECT * FROM t) v(x,y)",
    ] {
        assert_eq!(
            output_lineage(sql, &Options::new()).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }
}

#[test]
fn field_projections_trace_the_root_column() {
    let opts = Options::new().schema([("t", ["r"])]);
    for expr in ["r.a", "t.r.a", "t.r.a.b"] {
        let sql = format!("SELECT {expr} AS x FROM t");
        assert_eq!(
            output_lineage(&sql, &opts).unwrap().unwrap(),
            [col("x", &[("t", &["r"])])]
        );
    }
}

#[test]
fn windows_and_scalar_subqueries() {
    let sql = "SELECT SUM(t.x) OVER w AS s, (SELECT MAX(u.y) FROM u WHERE u.id=t.id) AS m FROM t WINDOW w AS (PARTITION BY t.p ORDER BY t.o)";
    assert_eq!(
        output_lineage(sql, &Options::new()).unwrap().unwrap(),
        [col("s", &[("t", &["o", "p", "x"])]), col("m", &[("u", &["y"])])]
    );
}
