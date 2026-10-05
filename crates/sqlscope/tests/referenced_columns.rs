#![allow(clippy::type_complexity)]

mod common;

use common::*;
use sqlscope::{column_origins, column_usages, referenced_columns, Clause, ColumnUsage, ErrorKind, Options};

fn meta() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("hive.raw.users", vec!["id", "name", "email"]),
        ("hive.raw.orders", vec!["oid", "uid", "amt", "status"]),
        ("c.s.other", vec!["o1", "o2", "o3"]),
    ]
}

fn with_meta(opts: Options) -> Options {
    opts.schema(meta())
}

fn assert_refs(sql: &str, expected: &[(&str, &[&str])], opts: Options) {
    let got = referenced_columns(sql, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(got, map(expected), "\nsql: {sql}");
}

fn assert_usages(sql: &str, expected: &[(&str, &str, Clause)], opts: Options) {
    let got = column_usages(sql, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut want: Vec<ColumnUsage> = expected
        .iter()
        .map(|(table, column, clause)| ColumnUsage {
            table: table.to_string(),
            column: column.to_string(),
            clause: *clause,
        })
        .collect();
    want.sort();
    assert_eq!(got, want, "\nsql: {sql}");
}

fn t() -> Options {
    options("trino")
}

use Clause::*;

#[test]
fn filter_positions_are_included() {
    let cases: Vec<(&str, &[(&str, &[&str])], Vec<(&str, &str, Clause)>)> = vec![
        (
            "SELECT a FROM t WHERE b > 1",
            &[("t", &["a", "b"])],
            vec![("t", "a", Select), ("t", "b", Where)],
        ),
        (
            "SELECT a, count(*) FROM t GROUP BY a, g",
            &[("t", &["a", "g"])],
            vec![("t", "a", Select), ("t", "a", GroupBy), ("t", "g", GroupBy)],
        ),
        (
            "SELECT a + b AS x FROM t GROUP BY x",
            &[("t", &["a", "b"])],
            vec![
                ("t", "a", Select),
                ("t", "a", GroupBy),
                ("t", "b", Select),
                ("t", "b", GroupBy),
            ],
        ),
        (
            "SELECT a, b FROM t GROUP BY 1, b",
            &[("t", &["a", "b"])],
            vec![
                ("t", "a", Select),
                ("t", "a", GroupBy),
                ("t", "b", Select),
                ("t", "b", GroupBy),
            ],
        ),
        (
            "SELECT a FROM t GROUP BY a HAVING sum(h) > 1",
            &[("t", &["a", "h"])],
            vec![("t", "a", Select), ("t", "a", GroupBy), ("t", "h", Having)],
        ),
        (
            "SELECT a FROM t ORDER BY o",
            &[("t", &["a", "o"])],
            vec![("t", "a", Select), ("t", "o", OrderBy)],
        ),
        (
            "SELECT a AS x FROM t ORDER BY x",
            &[("t", &["a"])],
            vec![("t", "a", Select), ("t", "a", OrderBy)],
        ),
        (
            "SELECT a, b FROM t ORDER BY b + 1",
            &[("t", &["a", "b"])],
            vec![("t", "a", Select), ("t", "b", Select), ("t", "b", OrderBy)],
        ),
        (
            "SELECT a, row_number() OVER (PARTITION BY b ORDER BY c) rn FROM t",
            &[("t", &["a", "b", "c"])],
            vec![("t", "a", Select), ("t", "b", Select), ("t", "c", Select)],
        ),
        (
            "SELECT sum(a) OVER w FROM t WINDOW w AS (PARTITION BY p ORDER BY o)",
            &[("t", &["a", "o", "p"])],
            vec![("t", "a", Select), ("t", "p", Window), ("t", "o", Window)],
        ),
    ];
    for (sql, refs, usages) in cases {
        assert_refs(sql, refs, t());
        assert_usages(sql, &usages, t());
    }
}

#[test]
fn rejects_multiple_statements() {
    let sql = "SELECT a FROM t; SELECT secret FROM restricted";
    assert_eq!(
        referenced_columns(sql, &t()).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(column_usages(sql, &t()).unwrap_err().kind(), ErrorKind::Unsupported);
    assert_eq!(column_origins(sql, &t()).unwrap_err().kind(), ErrorKind::Unsupported);
}

#[test]
fn superset_of_origins() {
    let sql = "SELECT order_id FROM hive.raw.orders WHERE status = 'X'";
    assert_eq!(
        column_origins(sql, &t()).unwrap(),
        map(&[("hive.raw.orders", &["order_id"])])
    );
    assert_refs(sql, &[("hive.raw.orders", &["order_id", "status"])], t());
}

#[test]
fn qualify_clause() {
    let sf = || options("snowflake");
    let sql = "SELECT a FROM t QUALIFY row_number() OVER (PARTITION BY p ORDER BY q) = 1";
    assert_refs(sql, &[("t", &["a", "p", "q"])], sf());
    assert_usages(
        sql,
        &[("t", "a", Select), ("t", "p", Qualify), ("t", "q", Qualify)],
        sf(),
    );
    let sql = "SELECT a, row_number() OVER (PARTITION BY p ORDER BY q) AS rn FROM t QUALIFY rn = 1";
    assert_refs(sql, &[("t", &["a", "p", "q"])], sf());
    assert_usages(
        sql,
        &[
            ("t", "a", Select),
            ("t", "p", Select),
            ("t", "p", Qualify),
            ("t", "q", Select),
            ("t", "q", Qualify),
        ],
        sf(),
    );
    assert_refs(
        "SELECT a FROM t QUALIFY row_number() OVER (PARTITION BY p ORDER BY q DESC) = 1",
        &[("t", &["a", "p", "q"])],
        sf(),
    );
}

#[test]
fn dialect_clauses() {
    for (dialect, sql, column, clause) in [
        ("spark", "SELECT a FROM t SORT BY s", "s", SortBy),
        ("spark", "SELECT a FROM t DISTRIBUTE BY d", "d", DistributeBy),
        ("spark", "SELECT a FROM t CLUSTER BY c", "c", ClusterBy),
        (
            "oracle",
            "SELECT a FROM t CONNECT BY PRIOR id = parent_id",
            "id",
            ConnectBy,
        ),
    ] {
        let mut expected = vec![("t", "a", Select), ("t", column, clause)];
        if clause == ConnectBy {
            expected.push(("t", "parent_id", ConnectBy));
        }
        assert_usages(sql, &expected, options(dialect));
    }
}

#[test]
fn joins() {
    let sql = "SELECT u.name FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid WHERE o.status = 'X' GROUP BY u.name";
    assert_refs(
        sql,
        &[
            ("hive.raw.users", &["id", "name"]),
            ("hive.raw.orders", &["status", "uid"]),
        ],
        with_meta(t()),
    );
    assert_usages(
        sql,
        &[
            ("hive.raw.users", "name", Select),
            ("hive.raw.users", "name", GroupBy),
            ("hive.raw.users", "id", JoinOn),
            ("hive.raw.orders", "uid", JoinOn),
            ("hive.raw.orders", "status", Where),
        ],
        with_meta(t()),
    );
    assert_refs(
        "SELECT name FROM hive.raw.users JOIN hive.raw.orders ON id = uid WHERE status = 'X'",
        &[
            ("hive.raw.users", &["id", "name"]),
            ("hive.raw.orders", &["status", "uid"]),
        ],
        with_meta(t()),
    );
    assert_refs(
        "SELECT name FROM a JOIN b ON a.k = b.k WHERE status = 'X'",
        &[("a", &["k", "name", "status"]), ("b", &["k", "name", "status"])],
        t(),
    );
    assert_refs(
        "SELECT hive.raw.users.id, hive.stage.users.email FROM hive.raw.users JOIN hive.stage.users ON hive.raw.users.id = hive.stage.users.id",
        &[("hive.raw.users", &["id"]), ("hive.stage.users", &["email", "id"])],
        t().schema([("hive.raw.users", vec!["id", "name"]), ("hive.stage.users", vec!["id", "email"])]),
    );
    assert_refs(
        "SELECT a, b FROM t WHERE c > 1 GROUP BY a, b",
        &[("t", &["a", "b", "c"])],
        t(),
    );
}

#[test]
fn ctes() {
    let sql = "WITH c AS (SELECT a, b FROM t) SELECT c.a FROM c WHERE c.b > 1";
    assert_refs(sql, &[("t", &["a", "b"])], t());
    assert_usages(sql, &[("t", "a", Select), ("t", "b", Select), ("t", "b", Where)], t());
    for (sql, expected) in [
        (
            "WITH c AS (SELECT a FROM t WHERE z > 1) SELECT a FROM c",
            &[("t", &["a", "z"][..])][..],
        ),
        (
            "WITH c(x, y) AS (SELECT a, b FROM t) SELECT x FROM c WHERE y > 1",
            &[("t", &["a", "b"][..])],
        ),
        (
            "WITH x AS (SELECT a FROM t), y AS (SELECT b FROM r) SELECT x.a, y.b FROM x, y",
            &[("t", &["a"][..]), ("r", &["b"][..])],
        ),
        (
            "WITH c AS (SELECT a, b FROM t) SELECT c1.a FROM c c1 JOIN c c2 ON c1.a = c2.b",
            &[("t", &["a", "b"][..])],
        ),
        (
            "WITH a AS (SELECT x, y FROM t WHERE w > 0), b AS (SELECT x FROM a WHERE y > 0) SELECT x FROM b",
            &[("t", &["w", "x", "y"][..])],
        ),
        (
            "WITH c AS (SELECT a, b, d FROM t) SELECT c1.a FROM c c1 JOIN c c2 ON c1.b = c2.d",
            &[("t", &["a", "b", "d"][..])],
        ),
        ("WITH t AS (SELECT a FROM t) SELECT a FROM t", &[("t", &["a"][..])]),
        (
            "WITH orders AS (SELECT uid FROM hive.raw.orders WHERE status = 'X') SELECT uid FROM orders",
            &[("hive.raw.orders", &["status", "uid"][..])],
        ),
        (
            "WITH RECURSIVE c AS (SELECT a FROM t UNION ALL SELECT c.a FROM c WHERE c.a > 0) SELECT a FROM c",
            &[("t", &["a"][..])],
        ),
    ] {
        assert_refs(sql, expected, t());
    }
    let got = referenced_columns(
        "WITH a AS (SELECT x FROM b), b AS (SELECT y FROM t) SELECT x FROM a",
        &t(),
    )
    .unwrap();
    assert!(got.contains_key("b"), "{got:?}");
}

#[test]
fn subqueries() {
    for (sql, expected) in [
        (
            "SELECT s.x FROM (SELECT a AS x, b FROM t WHERE c > 1) s WHERE s.x > 0",
            &[("t", &["a", "b", "c"][..])][..],
        ),
        (
            "SELECT a, (SELECT max(x) FROM r) m FROM t",
            &[("t", &["a"][..]), ("r", &["x"][..])],
        ),
        (
            "SELECT a FROM t WHERE b IN (SELECT k FROM r WHERE v > 1)",
            &[("t", &["a", "b"][..]), ("r", &["k", "v"][..])],
        ),
        (
            "SELECT a FROM t WHERE b NOT IN (SELECT k FROM r)",
            &[("t", &["a", "b"][..]), ("r", &["k"][..])],
        ),
        (
            "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM r WHERE r.k = t.a)",
            &[("t", &["a"][..]), ("r", &["k"][..])],
        ),
        (
            "SELECT a FROM t WHERE b > (SELECT max(x) FROM r WHERE r.k = t.a)",
            &[("t", &["a", "b"][..]), ("r", &["k", "x"][..])],
        ),
        (
            "SELECT s.x, o.amt FROM (SELECT a AS x FROM t WHERE c > 1) s JOIN o ON s.x = o.k",
            &[("t", &["a", "c"][..]), ("o", &["amt", "k"][..])],
        ),
        (
            "SELECT z FROM (SELECT y AS z FROM (SELECT x AS y FROM t WHERE w > 0) i WHERE i.y > 0) o WHERE o.z > 0",
            &[("t", &["w", "x"][..])],
        ),
    ] {
        assert_refs(sql, expected, t());
    }
    let sql = "SELECT (SELECT max(x) FROM r) AS m FROM t ORDER BY m";
    assert_refs(sql, &[("t", &[]), ("r", &["x"])], t());
    assert_usages(sql, &[("r", "x", Select), ("r", "x", OrderBy)], t());
}

#[test]
fn set_operations() {
    for (sql, expected) in [
        (
            "SELECT a FROM t WHERE b > 1 UNION ALL SELECT c FROM r WHERE d < 2",
            &[("t", &["a", "b"][..]), ("r", &["c", "d"][..])][..],
        ),
        (
            "SELECT a FROM t WHERE x > 1 INTERSECT SELECT b FROM r WHERE y < 2",
            &[("t", &["a", "x"][..]), ("r", &["b", "y"][..])],
        ),
        (
            "SELECT a FROM t EXCEPT SELECT b FROM r",
            &[("t", &["a"][..]), ("r", &["b"][..])],
        ),
        (
            "WITH c AS (SELECT a, b FROM t) SELECT a FROM c WHERE b > 1 UNION ALL SELECT x FROM r WHERE y < 2",
            &[("t", &["a", "b"][..]), ("r", &["x", "y"][..])],
        ),
        (
            "SELECT a FROM t WHERE p > 0 UNION SELECT b FROM r EXCEPT SELECT c FROM s WHERE q < 1",
            &[("t", &["a", "p"][..]), ("r", &["b"][..]), ("s", &["c", "q"][..])],
        ),
    ] {
        assert_refs(sql, expected, t());
    }
    let sql = "SELECT a FROM t UNION ALL SELECT b FROM r ORDER BY a";
    assert_refs(sql, &[("t", &["a"]), ("r", &["b"])], t());
    assert_usages(
        sql,
        &[
            ("t", "a", Select),
            ("t", "a", OrderBy),
            ("r", "b", Select),
            ("r", "b", OrderBy),
        ],
        t(),
    );
}

#[test]
fn stars() {
    assert_refs("SELECT * FROM t WHERE id > 0", &[("t", &["*", "id"])], t());
    let sql = "SELECT * FROM hive.raw.users WHERE id > 0";
    assert_refs(sql, &[("hive.raw.users", &["email", "id", "name"])], with_meta(t()));
    assert_usages(
        sql,
        &[
            ("hive.raw.users", "id", Select),
            ("hive.raw.users", "name", Select),
            ("hive.raw.users", "email", Select),
            ("hive.raw.users", "id", Where),
        ],
        with_meta(t()),
    );
    assert_refs(
        "SELECT u.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
        &[
            ("hive.raw.users", &["email", "id", "name"]),
            ("hive.raw.orders", &["uid"]),
        ],
        with_meta(t()),
    );
    assert_refs(
        "SELECT * FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
        &[
            ("hive.raw.users", &["email", "id", "name"]),
            ("hive.raw.orders", &["amt", "oid", "status", "uid"]),
        ],
        with_meta(t()),
    );
    assert_refs(
        "SELECT * FROM hive.raw.users u JOIN unknown_tbl x ON u.id = x.k",
        &[("hive.raw.users", &["id", "name"]), ("unknown_tbl", &["*", "k"])],
        t().schema([("hive.raw.users", vec!["id", "name"])]),
    );
    let got = referenced_columns("SELECT x.* FROM t", &t()).unwrap();
    assert!(!got["t"].is_empty(), "{got:?}");
    let got = referenced_columns("SELECT a FROM t WHERE EXISTS (SELECT t.* FROM r WHERE r.k = t.a)", &t()).unwrap();
    assert!(got["t"].contains(&"*".to_string()), "{got:?}");
}

#[test]
fn qualified_references() {
    assert_refs(
        "SELECT hive.raw.users.id FROM hive.raw.users WHERE hive.raw.users.email IS NOT NULL",
        &[("hive.raw.users", &["email", "id"])],
        with_meta(t()),
    );
    assert_refs(
        "SELECT o.amt FROM hive.raw.orders AS o WHERE o.status = 'X'",
        &[("hive.raw.orders", &["amt", "status"])],
        with_meta(t()),
    );
    assert_refs(
        "SELECT c.s.t1.a FROM c.s.t1 JOIN c.s.t2 ON c.s.t1.k = c.s.t2.k WHERE c.s.t2.f > 0",
        &[("c.s.t1", &["a", "k"]), ("c.s.t2", &["f", "k"])],
        t(),
    );
    let sql = "SELECT iceberg.rda_launch_to_engage.interaction.interaction_id, hive.rda_launch_to_engage.interaction.legacy_id \
        FROM iceberg.rda_launch_to_engage.interaction JOIN hive.rda_launch_to_engage.interaction \
        ON iceberg.rda_launch_to_engage.interaction.interaction_id = hive.rda_launch_to_engage.interaction.legacy_id \
        WHERE iceberg.rda_launch_to_engage.interaction.status = 'active'";
    let ice = "iceberg.rda_launch_to_engage.interaction";
    let hive = "hive.rda_launch_to_engage.interaction";
    assert_refs(
        sql,
        &[(ice, &["interaction_id", "status"]), (hive, &["legacy_id"])],
        t(),
    );
    assert_usages(
        sql,
        &[
            (ice, "interaction_id", Select),
            (ice, "interaction_id", JoinOn),
            (ice, "status", Where),
            (hive, "legacy_id", Select),
            (hive, "legacy_id", JoinOn),
        ],
        t(),
    );
}

#[test]
fn ddl_wrappers_and_empty_results() {
    for (sql, expected) in [
        (
            "CREATE VIEW v AS SELECT a FROM t WHERE b > 1",
            &[("t", &["a", "b"][..])][..],
        ),
        (
            "CREATE TABLE d AS SELECT a FROM t WHERE b > 1",
            &[("t", &["a", "b"][..])],
        ),
        ("INSERT INTO d SELECT a FROM t WHERE b > 1", &[("t", &["a", "b"][..])]),
        (
            "CREATE VIEW v AS WITH c AS (SELECT a, b FROM t WHERE z > 0) SELECT c.a FROM c JOIN r ON c.b = r.k",
            &[("t", &["a", "b", "z"][..]), ("r", &["k"][..])],
        ),
        (
            "INSERT INTO d (x, y) SELECT a, b FROM t WHERE c > 1",
            &[("t", &["a", "b", "c"][..])],
        ),
        (
            r#"SELECT "Order" FROM t WHERE "User" > 1"#,
            &[("t", &["Order", "User"][..])],
        ),
    ] {
        assert_refs(sql, expected, t());
    }
    assert_refs("SELECT 1 FROM t", &[("t", &[])], t());
    assert_usages("SELECT 1 FROM t", &[], t());
    for sql in ["DROP TABLE t", "CREATE TABLE t (a INT, b INT)"] {
        assert!(referenced_columns(sql, &t()).unwrap().is_empty());
        assert!(column_usages(sql, &t()).unwrap().is_empty());
    }
}

#[test]
fn expressions() {
    for (sql, expected) in [
        ("SELECT CASE WHEN a > 1 THEN b ELSE c END FROM t", &["a", "b", "c"][..]),
        ("SELECT coalesce(a, b) FROM t", &["a", "b"]),
        ("SELECT a + b * c FROM t", &["a", "b", "c"]),
        ("SELECT a FROM t WHERE x BETWEEN y AND z", &["a", "x", "y", "z"]),
        ("SELECT a FROM t WHERE x IN (y, z)", &["a", "x", "y", "z"]),
        ("SELECT a + b FROM t GROUP BY a + b", &["a", "b"]),
        (
            "SELECT sum(amt) OVER (PARTITION BY p ORDER BY o ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
            &["amt", "o", "p"],
        ),
        (
            "SELECT g FROM t GROUP BY g HAVING max(h) - min(h2) > 3",
            &["g", "h", "h2"],
        ),
        (
            "SELECT l.a, r.b FROM t l JOIN t r ON l.k1 = r.k2 WHERE l.w > 0 ORDER BY r.o",
            &["a", "b", "k1", "k2", "o", "w"],
        ),
        ("SELECT a.x, b.y FROM t a JOIN t b ON a.id = b.id", &["id", "x", "y"]),
        ("SELECT a, b FROM t ORDER BY 1, b", &["a", "b"]),
    ] {
        assert_refs(sql, &[("t", expected)], t());
    }
    let sql = "SELECT b, a, b FROM t WHERE a > 1 AND b < 2 ORDER BY a";
    assert_refs(sql, &[("t", &["a", "b"])], t());
    assert_usages(
        sql,
        &[
            ("t", "b", Select),
            ("t", "a", Select),
            ("t", "b", Where),
            ("t", "a", Where),
            ("t", "a", OrderBy),
        ],
        t(),
    );
    assert_usages(
        "SELECT a, b FROM t ORDER BY 1, b",
        &[
            ("t", "a", Select),
            ("t", "a", OrderBy),
            ("t", "b", Select),
            ("t", "b", OrderBy),
        ],
        t(),
    );
}

#[test]
fn from_constructs() {
    let sql = "SELECT a FROM t JOIN r USING (k)";
    assert_refs(sql, &[("t", &["a", "k"]), ("r", &["a", "k"])], t());
    assert_usages(
        sql,
        &[
            ("t", "a", Select),
            ("r", "a", Select),
            ("t", "k", JoinUsing),
            ("r", "k", JoinUsing),
        ],
        t(),
    );
    let sql = "SELECT i FROM t, UNNEST(t.arr) AS x(i)";
    assert_refs(sql, &[("t", &["arr", "i"])], t());
    assert_usages(sql, &[("t", "arr", From), ("t", "i", Select)], t());
    assert_refs(
        "SELECT * FROM (SELECT region, amt FROM s) PIVOT (sum(amt) FOR region IN ('A'))",
        &[("s", &["amt", "region"])],
        t(),
    );
    let sql = "SELECT e FROM t LATERAL VIEW explode(t.arr) tbl AS e";
    assert_refs(sql, &[("t", &["arr", "e"])], options("spark"));
    assert_usages(sql, &[("t", "arr", LateralView), ("t", "e", Select)], options("spark"));
    assert_refs(
        "SELECT u.name FROM hive.raw.users u JOIN hive.raw.orders o USING (id)",
        &[("hive.raw.users", &["id", "name"]), ("hive.raw.orders", &["id"])],
        t().schema([
            ("hive.raw.users", vec!["id", "name"]),
            ("hive.raw.orders", vec!["id", "amt"]),
        ]),
    );
}

#[test]
fn unresolvable_references_are_broadcast() {
    assert_refs("SELECT x.a FROM t", &[("t", &["a"])], t());
    assert_refs(
        "SELECT name FROM a JOIN b ON a.k = b.k WHERE foo > 1",
        &[("a", &["foo", "k", "name"]), ("b", &["foo", "k", "name"])],
        t(),
    );
    assert_refs(
        "SELECT u.id FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid WHERE foo > 1",
        &[("hive.raw.users", &["foo", "id"]), ("hive.raw.orders", &["foo", "uid"])],
        with_meta(t()),
    );
    assert_refs("SELECT s.x FROM (SELECT * FROM t) s", &[("t", &["*", "x"])], t());
    assert_refs(
        "SELECT k FROM a JOIN b ON a.x = b.y",
        &[("a", &["k", "x"]), ("b", &["k", "y"])],
        t().schema([("a", vec!["k", "x"]), ("b", vec!["k", "y"])]),
    );
    assert_refs(
        "SELECT u.id FROM hive.raw.users u WHERE EXISTS (SELECT 1 FROM hive.raw.orders o WHERE o.uid = u.id AND name = 'x')",
        &[("hive.raw.users", &["id", "name"]), ("hive.raw.orders", &["uid"])],
        with_meta(t()),
    );
}

#[test]
fn unrelated_schema_entries_are_ignored() {
    assert_refs(
        "SELECT a, b FROM foo",
        &[("foo", &["a", "b"])],
        t().schema([("foo", vec![]), ("other_table", vec!["a", "b", "c"])]),
    );
}

#[test]
fn dml() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])], Vec<(&str, &str, Clause)>)> = vec![
        ("trino", "DELETE FROM t WHERE a > 1", &[("t", &["a"])], vec![("t", "a", Where)]),
        (
            "postgres",
            "DELETE FROM t USING r WHERE t.id = r.id AND r.k > 1",
            &[("t", &["id"]), ("r", &["id", "k"])],
            vec![("t", "id", Where), ("r", "id", Where), ("r", "k", Where)],
        ),
        (
            "trino",
            "UPDATE t SET x = y + 1 WHERE a > 1",
            &[("t", &["a", "x", "y"])],
            vec![("t", "x", UpdateSetTarget), ("t", "y", UpdateSetValue), ("t", "a", Where)],
        ),
        (
            "postgres",
            "UPDATE t SET x = s.v FROM r s WHERE t.id = s.id",
            &[("t", &["id", "x"]), ("r", &["id", "v"])],
            vec![("t", "x", UpdateSetTarget), ("r", "v", UpdateSetValue), ("t", "id", Where), ("r", "id", Where)],
        ),
        (
            "trino",
            "MERGE INTO t USING r ON t.id = r.id WHEN MATCHED THEN UPDATE SET x = r.v WHEN NOT MATCHED THEN INSERT (a) VALUES (r.b)",
            &[("t", &["id"]), ("r", &["b", "id", "v"])],
            vec![("t", "id", MergeOn), ("r", "id", MergeOn), ("r", "v", MergeWhen), ("r", "b", MergeWhen)],
        ),
        (
            "trino",
            "MERGE INTO t USING (SELECT id, v FROM r WHERE z > 0) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET x = s.v",
            &[("t", &["id"]), ("r", &["id", "v", "z"])],
            vec![
                ("t", "id", MergeOn),
                ("r", "id", Select),
                ("r", "id", MergeOn),
                ("r", "v", Select),
                ("r", "v", MergeWhen),
                ("r", "z", Where),
            ],
        ),
    ];
    for (dialect, sql, refs, usages) in cases {
        assert_refs(sql, refs, options(dialect));
        assert_usages(sql, &usages, options(dialect));
    }
    assert_refs(
        "UPDATE t SET t.x = t.y + 1 WHERE t.a > 1",
        &[("t", &["a", "x", "y"])],
        options("mysql"),
    );
    assert_refs(
        "DELETE FROM t WHERE id IN (SELECT uid FROM r WHERE flag = 1)",
        &[("t", &["id"]), ("r", &["flag", "uid"])],
        t(),
    );
    assert_refs(
        "MERGE INTO t USING r ON t.id = r.id WHEN MATCHED AND r.flag = 1 THEN DELETE",
        &[("t", &["id"]), ("r", &["flag", "id"])],
        t(),
    );
}

#[test]
fn inner_projection_is_still_a_reference() {
    assert_refs(
        "WITH c AS (SELECT id, amount, unused FROM orders) SELECT id FROM c WHERE amount > 0",
        &[("orders", &["amount", "id", "unused"])],
        t(),
    );
}

/// ReferencedColumns is a per-table superset of ColumnOrigins.
#[test]
fn superset_invariant() {
    let trino_meta = trino_schema();
    let small = || {
        t().schema([
            ("hive.raw.users", vec!["id", "name", "email"]),
            ("hive.raw.orders", vec!["oid", "uid", "amt", "status"]),
        ])
    };
    let corpus: Vec<(&str, Options)> = vec![
        ("SELECT user_id FROM hive.raw.orders WHERE status = 'X'", trino_meta.clone()),
        ("SELECT u.user_name FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id WHERE o.status = 'PAID'", trino_meta.clone()),
        ("CREATE VIEW v AS SELECT o.amount FROM hive.raw.orders o WHERE o.order_date >= DATE '2024-01-01'", trino_meta.clone()),
        ("WITH p AS (SELECT order_id, user_id FROM hive.raw.orders WHERE status = 'PAID') SELECT order_id FROM p WHERE user_id > 0", trino_meta.clone()),
        ("SELECT user_id FROM hive.raw.users UNION SELECT user_id FROM hive.raw.orders", trino_meta.clone()),
        ("CREATE TABLE d AS SELECT o.amount FROM hive.raw.orders o GROUP BY o.amount HAVING count(o.order_id) > 1", trino_meta.clone()),
        ("INSERT INTO d SELECT u.user_name FROM hive.raw.users u WHERE u.email IS NOT NULL ORDER BY u.user_id", trino_meta.clone()),
        ("SELECT amount FROM hive.raw.orders o WHERE o.user_id IN (SELECT user_id FROM hive.raw.users WHERE email LIKE '%@x.com')", trino_meta.clone()),
        ("SELECT * FROM hive.raw.orders WHERE status = 'PAID'", trino_meta.clone()),
        ("SELECT o.amount, u.user_name FROM hive.raw.orders o JOIN hive.raw.users u ON o.user_id = u.user_id WHERE u.email IS NOT NULL GROUP BY o.amount, u.user_name", trino_meta.clone()),
        ("SELECT id FROM hive.raw.users WHERE email IS NOT NULL", small()),
        ("SELECT u.name, o.amt FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid", small()),
        ("SELECT name FROM hive.raw.users ORDER BY id", small()),
        ("SELECT status, sum(amt) FROM hive.raw.orders GROUP BY status HAVING count(oid) > 1", small()),
        ("WITH c AS (SELECT id, name FROM hive.raw.users WHERE email LIKE '%x') SELECT name FROM c WHERE id > 0", small()),
        ("SELECT id FROM hive.raw.users UNION ALL SELECT uid FROM hive.raw.orders", small()),
        ("SELECT s.n FROM (SELECT name AS n, id FROM hive.raw.users) s WHERE s.id > 1", small()),
        ("SELECT id, row_number() OVER (PARTITION BY name ORDER BY email) FROM hive.raw.users", small()),
        ("SELECT a.id FROM hive.raw.users a JOIN hive.raw.users b ON a.email = b.name", small()),
    ];
    for (sql, opts) in corpus {
        let origins = column_origins(sql, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let referenced = referenced_columns(sql, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(!origins.is_empty() && !referenced.is_empty(), "{sql}");
        for (table, columns) in &origins {
            let refs = referenced
                .get(table)
                .unwrap_or_else(|| panic!("{sql}: {table} missing from {referenced:?}"));
            for column in columns {
                assert!(refs.contains(column), "{sql}: {table}.{column} not in {refs:?}");
            }
        }
    }
}
