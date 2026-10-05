#![allow(clippy::type_complexity)]

mod common;

use common::*;
use sqlscope::{output_columns, ErrorKind, Options};

fn view_meta(opts: Options) -> Options {
    opts.schema([
        ("hive.raw.users", vec!["id", "name", "email"]),
        ("hive.raw.orders", vec!["oid", "uid", "amt"]),
        ("c.s.other", vec!["o1", "o2", "o3"]),
    ])
}

fn users(opts: Options) -> Options {
    opts.schema([("hive.raw.users", vec!["id", "name", "email"])])
}

fn check(sql: &str, expected: &[&str], opts: Options) {
    let got = output_columns(sql, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(
        got,
        Some(expected.iter().map(|s| s.to_string()).collect()),
        "\nsql: {sql}"
    );
}

fn unsupported(sql: &str, opts: Options) {
    let err = output_columns(sql, &opts).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported, "{sql}: {err}");
}

fn t() -> Options {
    options("trino")
}

#[test]
fn real_view_with_quoted_aliases() {
    let mut select =
        String::from("hco_province_cn, hco_tier_cn, standard_department_nm, is_core_department, bu, brand");
    let mut expected: Vec<String> = [
        "hco_province_cn",
        "hco_tier_cn",
        "standard_department_nm",
        "is_core_department",
        "bu",
        "brand",
    ]
    .map(String::from)
    .to_vec();
    for prefix in ["hcp_hco", "hcp"] {
        for year in [2025, 2026] {
            for kind in ["Target", "YTD Target"] {
                for quarter in 1..=4 {
                    let alias = format!("{prefix} {year} Q{quarter} {kind}");
                    select.push_str(&format!(
                        ", count(DISTINCT (CASE WHEN (yearmonth BETWEEN '{year}01' AND '{year}03') THEN concat(hcp_etms_cd, hco_etms_cd) END)) \"{alias}\""
                    ));
                    expected.push(alias);
                }
            }
        }
    }
    let sql = format!(
        "CREATE VIEW vdm_mda.weiava.target_hcp_cnt AS\nSELECT {select} FROM iceberg.rda_launch_to_engage.target_hcp WHERE (yearmonth >= '202501') GROUP BY 1, 2, 3, 4, 5, 6 ORDER BY 1 ASC, 2 ASC"
    );
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    check(&sql, &expected, t());
    check(
        &sql.replace("AS\nSELECT", "SECURITY DEFINER AS\nSELECT"),
        &expected,
        t(),
    );
}

#[test]
fn projection_names() {
    let cases: Vec<(&str, &[&str])> = vec![
        ("CREATE VIEW c.s.v AS SELECT a, count(*), sum(x) FROM t GROUP BY 1", &["a", "_col1", "_col2"]),
        ("CREATE VIEW c.s.v (x1, x2, x3) AS SELECT a, b, count(*) FROM t GROUP BY 1, 2", &["x1", "x2", "x3"]),
        ("CREATE VIEW c.s.v (x1, x2) AS SELECT a, b, c FROM t", &["x1", "x2"]),
        ("SELECT a AS x, b FROM t", &["x", "b"]),
        ("SELECT a, count(*) FROM t GROUP BY 1 UNION ALL SELECT c, d FROM r", &["a", "_col1"]),
        ("SELECT a AS l1, b AS l2 FROM t UNION ALL SELECT c, d FROM r", &["l1", "l2"]),
        ("CREATE VIEW c.s.v AS WITH paid AS (SELECT order_id, amount FROM orders WHERE status = 'PAID') SELECT order_id AS oid, amount AS amt FROM paid", &["oid", "amt"]),
        ("SELECT * FROM hive.raw.users", &["*"]),
        ("CREATE TABLE c.s.t AS SELECT a, b, count(*) FROM r GROUP BY 1, 2", &["a", "b", "_col2"]),
        ("CREATE TABLE c.s.t (x1, x2) AS SELECT a, b FROM r", &["x1", "x2"]),
        (r#"CREATE TABLE c.s.t AS SELECT a, count(*) "Total Cnt" FROM r GROUP BY 1"#, &["a", "Total Cnt"]),
        ("CREATE OR REPLACE TABLE c.s.t AS SELECT x AS y, z FROM r", &["y", "z"]),
        ("CREATE TABLE c.s.t (id BIGINT, name VARCHAR, created_at TIMESTAMP)", &["id", "name", "created_at"]),
        ("CREATE TABLE c.s.t (a INTEGER, b VARCHAR) WITH (format = 'PARQUET', partitioning = ARRAY['a'])", &["a", "b"]),
        ("CREATE TABLE c.s.t (id BIGINT NOT NULL COMMENT 'pk', name VARCHAR COMMENT 'n')", &["id", "name"]),
        ("CREATE TABLE IF NOT EXISTS c.s.t (a INT, b INT)", &["a", "b"]),
        ("CREATE TABLE c.s.t AS WITH c AS (SELECT a, b FROM r) SELECT a, count(*) FROM c GROUP BY 1", &["a", "_col1"]),
        ("CREATE MATERIALIZED VIEW c.s.mv AS SELECT a, sum(b) total FROM r GROUP BY 1", &["a", "total"]),
        ("CREATE OR REPLACE VIEW c.s.v AS SELECT a, b FROM r", &["a", "b"]),
        ("WITH cte AS (SELECT a, b FROM r) SELECT a AS x, b FROM cte", &["x", "b"]),
        ("WITH c1 AS (SELECT a FROM r), c2 AS (SELECT b FROM s) SELECT c1.a, c2.b FROM c1, c2", &["a", "b"]),
        ("WITH c(x, y) AS (SELECT a, b FROM r) SELECT x, y FROM c", &["x", "y"]),
        ("WITH c AS (SELECT a, b FROM r) SELECT a, count(*) FROM c GROUP BY 1 UNION ALL SELECT x, y FROM s", &["a", "_col1"]),
        ("CREATE VIEW c.s.v AS WITH c AS (SELECT a, b FROM r) SELECT a AS oid, b AS amt FROM c", &["oid", "amt"]),
        ("INSERT INTO c.s.t SELECT a, b FROM r", &["a", "b"]),
        ("INSERT INTO c.s.t (col1, col2) SELECT a, b FROM r", &["col1", "col2"]),
        ("INSERT INTO c.s.t (a, b) VALUES (1, 2), (3, 4)", &["a", "b"]),
        ("SELECT a AS x, b AS x FROM t", &["x", "x"]),
        ("SELECT count(*), a, sum(b) FROM t GROUP BY 2", &["_col0", "a", "_col2"]),
        ("SELECT a, count(*) FROM t GROUP BY 1", &["a", "_col1"]),
        (r#"SELECT a AS "MiXeD Case", b AS "with""quote" FROM t"#, &["MiXeD Case", "with\"quote"]),
        ("SELECT count(*) FROM hive.raw.users", &["_col0"]),
    ];
    for (sql, expected) in cases {
        check(sql, expected, t());
    }
    check("SELECT a AS `My Col`, b FROM t", &["My Col", "b"], options("mysql"));
    check(
        "CREATE TABLE t (id BIGINT, name VARCHAR(10), PRIMARY KEY (id), KEY idx_name (name))",
        &["id", "name"],
        options("mysql"),
    );
    check(
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 3) SELECT n AS depth FROM r",
        &["depth"],
        options("postgresql"),
    );
    check(
        "SELECT 1 AS a UNION ALL BY NAME SELECT 2 AS b",
        &["a", "b"],
        options("duckdb"),
    );
}

#[test]
fn wildcard_expansion() {
    let cases: Vec<(&str, &[&str], Options)> = vec![
        ("SELECT * FROM hive.raw.users", &[], t().schema([("hive.raw.users", Vec::<&str>::new())])),
        ("SELECT u.* FROM hive.raw.users u", &[], t().schema([("hive.raw.users", Vec::<&str>::new())])),
        ("SELECT u.name, o.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid", &["name", "oid", "uid", "amt"], view_meta(t())),
        ("CREATE VIEW hive.analytics.v AS SELECT * FROM hive.raw.users", &["id", "name", "email"], users(t())),
        ("CREATE VIEW hive.analytics.v AS SELECT * FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid", &["id", "name", "email", "oid", "uid", "amt"], view_meta(t())),
        ("CREATE VIEW hive.analytics.v AS SELECT u.*, o.amt FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid", &["id", "name", "email", "amt"], view_meta(t())),
        ("CREATE VIEW hive.analytics.v AS SELECT * FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid", &["*"], users(t())),
        ("WITH c AS (SELECT * FROM hive.raw.users) SELECT * FROM c", &["id", "name", "email"], users(t())),
        ("WITH c AS (SELECT * FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid) SELECT * FROM c", &["id", "name", "email", "oid", "uid", "amt"], view_meta(t())),
        ("WITH c AS (SELECT u.*, o.amt FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid) SELECT * FROM c", &["id", "name", "email", "amt"], view_meta(t())),
        ("WITH c AS (SELECT * FROM hive.raw.users) SELECT c.* FROM c", &["id", "name", "email"], users(t())),
        ("WITH c AS (SELECT * FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid) SELECT * FROM c", &["*"], users(t())),
        ("CREATE VIEW hive.analytics.v AS WITH c AS (SELECT * FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid) SELECT * FROM c", &["id", "name", "email", "oid", "uid", "amt"], view_meta(t())),
        ("INSERT INTO c.s.t SELECT * FROM hive.raw.users", &["id", "name", "email"], users(t())),
        ("INSERT INTO c.s.t (c1, c2, c3) SELECT * FROM hive.raw.users", &["c1", "c2", "c3"], view_meta(t())),
        ("SELECT email, id FROM hive.raw.users", &["email", "id"], view_meta(t())),
        ("SELECT o.amt, u.name, o.oid FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid", &["amt", "name", "oid"], view_meta(t())),
        ("SELECT o.amt, u.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid", &["amt", "id", "name", "email"], view_meta(t())),
        ("SELECT o.*, u.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid", &["oid", "uid", "amt", "id", "name", "email"], view_meta(t())),
        ("SELECT id, id FROM hive.raw.users", &["id", "id"], view_meta(t())),
        ("SELECT * FROM (SELECT id AS x, name AS y FROM hive.raw.users) s", &["x", "y"], view_meta(t())),
        ("WITH c AS (SELECT email, id FROM hive.raw.users) SELECT * FROM c", &["email", "id"], view_meta(t())),
        ("SELECT * FROM hive.raw.users UNION ALL SELECT oid, uid, amt FROM hive.raw.orders", &["id", "name", "email"], view_meta(t())),
        ("SELECT id, name, email FROM hive.raw.users UNION ALL SELECT * FROM hive.raw.orders", &["id", "name", "email"], view_meta(t())),
        ("SELECT count(*) AS n FROM hive.raw.users", &["n"], view_meta(t())),
        ("SELECT u.id FROM hive.raw.users u", &["id"], view_meta(t())),
        ("SELECT * FROM hive.raw.users", &["*"], t().schema(Vec::<(&str, Vec<&str>)>::new())),
        ("SELECT * FROM hive.raw.users", &["*"], t().schema([("hive.raw.orders", vec!["oid"])])),
        ("SELECT * FROM foo", &[], t().schema([("foo", vec![]), ("other_table", vec!["a", "b", "c"])])),
        ("SELECT * FROM foo", &["*"], t().schema([("other_table", vec!["a", "b"])])),
        ("SELECT * FROM iceberg.rda_launch_to_engage.interaction", &["interaction_id", "status"], t().schema([
            ("iceberg.rda_launch_to_engage.interaction", vec!["interaction_id", "status"]),
            ("hive.rda_launch_to_engage.interaction", vec!["legacy_id"]),
        ])),
    ];
    for (sql, expected, opts) in cases {
        check(sql, expected, opts);
    }
    let got = output_columns("SELECT u.*, u.id + 1 FROM hive.raw.users u", &view_meta(t()))
        .unwrap()
        .unwrap();
    assert_eq!(got.len(), 4);
    assert!(got[3] == "_col1" || got[3] == "_col3", "{got:?}");
}

#[test]
fn projection_order_follows_query() {
    let opts = || t().schema([("t", vec!["z", "a"])]);
    check("WITH c AS (SELECT a, z FROM t) SELECT * FROM c", &["a", "z"], opts());
    check("SELECT t.*, 1, t.* FROM t", &["z", "a", "_col2", "z", "a"], opts());
    check("SELECT 1 AS same, a AS same FROM t", &["same", "same"], opts());
    check("SELECT a AS _col_0 FROM t", &["_col_0"], opts());
    let empty = || t().schema([("empty", Vec::<&str>::new())]);
    check("SELECT 1, t.* FROM t", &["_col0", "*"], empty());
    check("SELECT 1, e.* FROM empty e", &["_col0"], empty());
    check("SELECT e.*, t.* FROM empty e CROSS JOIN t", &["*"], empty());
}

#[test]
fn like_clauses() {
    unsupported("CREATE TABLE c.s.t (LIKE c.s.other)", t());
    check(
        "CREATE TABLE c.s.t (LIKE c.s.other)",
        &["o1", "o2", "o3"],
        view_meta(t()),
    );
    check(
        "CREATE TABLE c.s.t (LIKE c.s.other INCLUDING PROPERTIES)",
        &["o1", "o2", "o3"],
        view_meta(t()),
    );
    check(
        "CREATE TABLE c.s.t (a INT, LIKE c.s.other, b VARCHAR)",
        &["a", "o1", "o2", "o3", "b"],
        view_meta(t()),
    );
    check("CREATE TABLE c.s.t (LIKE other)", &["o1", "o2", "o3"], view_meta(t()));
    check(
        "CREATE TABLE c.s.t (LIKE c.s.other)",
        &[],
        t().schema([("c.s.other", Vec::<&str>::new())]),
    );
    unsupported(
        "CREATE TABLE c.s.t (LIKE other)",
        t().schema([("a.b.other", vec!["a1"]), ("x.y.other", vec!["x1"])]),
    );
}

#[test]
fn unsupported_and_empty_statements() {
    unsupported("SELECT a FROM t; SELECT secret FROM restricted", t());
    unsupported("INSERT INTO c.s.t VALUES (1, 2)", t());
    unsupported("INSERT INTO c.s.t DEFAULT VALUES", options("postgresql"));
    assert_eq!(output_columns("DROP TABLE hive.x.t", &t()).unwrap(), None);
}

#[test]
fn row_field_access() {
    let got = output_columns("SELECT t.c.f FROM c.s.t t", &t()).unwrap().unwrap();
    assert!(got == ["f"] || got == ["_col0"], "{got:?}");
}
