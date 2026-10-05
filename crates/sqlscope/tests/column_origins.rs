#![allow(clippy::type_complexity)]

mod common;

use common::*;
use sqlscope::{column_origins, ErrorKind, Options};

fn check(sql: &str, expected: &[(&str, &[&str])], opts: &Options) {
    let got = column_origins(sql, opts).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(got, map(expected), "\nsql: {sql}");
}

fn lineage(sql: &str, expected: &[(&str, &[&str])]) {
    check(sql, expected, &trino_schema());
}

const ORDERS_ALL: &[&str] = &[
    "amount",
    "order_date",
    "order_id",
    "order_ts",
    "quantity",
    "status",
    "user_id",
];

#[test]
fn views_and_wildcards() {
    lineage(
        "CREATE VIEW hive.analytics.v_user_orders AS SELECT u.user_name, o.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id WHERE o.status = 'PAID' AND u.email IS NOT NULL",
        &[("hive.raw.orders", ORDERS_ALL), ("hive.raw.users", &["user_name"])],
    );
    lineage(
        "CREATE VIEW hive.analytics.v AS SELECT u.user_id AS a, o.user_id AS b FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id WHERE o.status IN ('PAID')",
        &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])],
    );
    lineage(
        "CREATE VIEW hive.analytics.v AS SELECT user_id FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id WHERE o.order_date >= DATE '2024-01-01'",
        &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])],
    );
    lineage(
        "CREATE VIEW hive.analytics.v AS SELECT o.amount * o.quantity AS gross, date_trunc('day', o.order_ts) AS d, u.user_name FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id WHERE o.status = 'PAID'",
        &[("hive.raw.orders", &["amount", "order_ts", "quantity"]), ("hive.raw.users", &["user_name"])],
    );
    lineage(
        "CREATE VIEW hive.analytics.v AS WITH paid_orders AS (SELECT o.order_id, o.user_id, p.paid_amount FROM hive.raw.orders o JOIN hive.raw.payments p ON o.order_id = p.order_id WHERE o.status = 'PAID' AND p.paid_at >= TIMESTAMP '2024-01-01 00:00:00') SELECT po.order_id, po.user_id, po.paid_amount FROM paid_orders po WHERE po.paid_amount > 0",
        &[("hive.raw.orders", &["order_id", "user_id"]), ("hive.raw.payments", &["paid_amount"])],
    );
}

#[test]
fn statement_types() {
    let cases: Vec<(&str, &[(&str, &[&str])])> = vec![
        ("SELECT user_id, user_name FROM hive.raw.users", &[("hive.raw.users", &["user_id", "user_name"])]),
        ("SELECT * FROM hive.raw.orders", &[("hive.raw.orders", ORDERS_ALL)]),
        ("SELECT order_id FROM hive.raw.orders WHERE status = 'X'", &[("hive.raw.orders", &["order_id"])]),
        ("SELECT u.user_name FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id", &[("hive.raw.orders", &[]), ("hive.raw.users", &["user_name"])]),
        ("WITH a AS (SELECT o.user_id, o.amount FROM hive.raw.orders o), b AS (SELECT a.user_id FROM a) SELECT b.user_id FROM b", &[("hive.raw.orders", &["user_id"])]),
        ("SELECT x.uid FROM (SELECT o.user_id AS uid FROM hive.raw.orders o) x", &[("hive.raw.orders", &["user_id"])]),
        ("SELECT row_number() OVER (PARTITION BY o.user_id ORDER BY o.order_ts) AS rn FROM hive.raw.orders o", &[("hive.raw.orders", &["order_ts", "user_id"])]),
        ("CREATE TABLE hive.x.t AS SELECT o.amount, o.user_id FROM hive.raw.orders o WHERE o.status = 'X'", &[("hive.raw.orders", &["amount", "user_id"])]),
        ("INSERT INTO hive.x.t SELECT o.amount FROM hive.raw.orders o WHERE o.status = 'X'", &[("hive.raw.orders", &["amount"])]),
        ("CREATE VIEW hive.x.v AS SELECT u.user_id FROM hive.raw.users u UNION SELECT o.user_id FROM hive.raw.orders o", &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])]),
    ];
    for (sql, expected) in cases {
        lineage(sql, expected);
    }
}

#[test]
fn set_operations() {
    let both = &[
        ("hive.raw.orders", &["user_id"][..]),
        ("hive.raw.users", &["user_id"][..]),
    ][..];
    let left_only = &[("hive.raw.orders", &[][..]), ("hive.raw.users", &["user_id"][..])][..];
    let three = &[
        ("hive.raw.orders", &["user_id"][..]),
        ("hive.raw.payments", &["order_id"][..]),
        ("hive.raw.users", &["user_id"][..]),
    ][..];
    for (sql, expected) in [
        ("SELECT user_id FROM hive.raw.users UNION SELECT user_id FROM hive.raw.orders", both),
        ("SELECT user_id FROM hive.raw.users UNION ALL SELECT user_id FROM hive.raw.orders", both),
        ("SELECT user_id FROM hive.raw.users EXCEPT SELECT user_id FROM hive.raw.orders", left_only),
        ("SELECT user_id FROM hive.raw.users INTERSECT SELECT user_id FROM hive.raw.orders", left_only),
        ("SELECT user_id FROM hive.raw.users EXCEPT SELECT user_id FROM hive.raw.users", &[("hive.raw.users", &["user_id"][..])][..]),
        ("SELECT user_id FROM hive.raw.users UNION (SELECT user_id FROM hive.raw.orders UNION ALL SELECT order_id FROM hive.raw.payments)", three),
        ("(SELECT user_id FROM hive.raw.users UNION SELECT user_id FROM hive.raw.orders) UNION SELECT order_id FROM hive.raw.payments", three),
        ("SELECT user_id FROM hive.raw.users UNION SELECT user_id FROM hive.raw.orders UNION SELECT order_id FROM hive.raw.payments", three),
        ("WITH candidates AS (SELECT user_id FROM hive.raw.users) SELECT user_id FROM candidates UNION ALL SELECT user_id FROM candidates", &[("hive.raw.users", &["user_id"][..])]),
        ("WITH c AS (SELECT user_id FROM hive.raw.users UNION SELECT user_id FROM hive.raw.orders) SELECT user_id FROM c", both),
    ] {
        lineage(sql, expected);
    }
}

#[test]
fn filter_only_positions_are_excluded() {
    let cases: Vec<(&str, &[(&str, &[&str])])> = vec![
        ("SELECT order_id FROM hive.raw.orders ORDER BY order_ts", &[("hive.raw.orders", &["order_id"])]),
        ("SELECT o.user_id, count(o.order_id) AS c FROM hive.raw.orders o GROUP BY o.user_id HAVING max(o.amount) > 10", &[("hive.raw.orders", &["order_id", "user_id"])]),
        ("SELECT status FROM hive.raw.orders WHERE status = 'X'", &[("hive.raw.orders", &["status"])]),
        ("SELECT CASE WHEN status = 'X' THEN amount ELSE quantity END AS v FROM hive.raw.orders", &[("hive.raw.orders", &["amount", "quantity", "status"])]),
        ("SELECT sum(o.amount) AS total FROM hive.raw.orders o GROUP BY o.status", &[("hive.raw.orders", &["amount"])]),
        ("SELECT max(o.amount) OVER (ORDER BY o.order_ts) AS m FROM hive.raw.orders o", &[("hive.raw.orders", &["amount", "order_ts"])]),
        ("SELECT o.amount FROM hive.raw.orders o WHERE o.user_id IN (SELECT u.user_id FROM hive.raw.users u)", &[("hive.raw.orders", &["amount"]), ("hive.raw.users", &[])]),
        ("SELECT a.user_name FROM hive.raw.users a JOIN hive.raw.users b ON a.user_id = b.user_id", &[("hive.raw.users", &["user_name"])]),
        ("WITH a AS (SELECT o.user_id, o.amount FROM hive.raw.orders o WHERE o.status = 'X'), b AS (SELECT a.user_id FROM a JOIN hive.raw.users u ON a.user_id = u.user_id) SELECT b.user_id FROM b", &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &[])]),
        ("SELECT y.uid FROM (SELECT x.uid FROM (SELECT o.user_id AS uid FROM hive.raw.orders o) x) y", &[("hive.raw.orders", &["user_id"])]),
        ("SELECT u.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id", &[("hive.raw.orders", &[]), ("hive.raw.users", &["email", "user_id", "user_name"])]),
        ("SELECT email FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id", &[("hive.raw.orders", &[]), ("hive.raw.users", &["email"])]),
    ];
    for (sql, expected) in cases {
        lineage(sql, expected);
    }
}

#[test]
fn outputs_without_source_columns() {
    let cases: Vec<(&str, &[(&str, &[&str])])> = vec![
        ("SELECT count(*) AS c FROM hive.raw.orders", &[("hive.raw.orders", &[])]),
        ("SELECT 1 AS one, 'a' AS lit FROM hive.raw.orders", &[("hive.raw.orders", &[])]),
        ("SELECT sum(amount) FROM hive.raw.orders GROUP BY status", &[("hive.raw.orders", &["amount"])]),
        (
            "SELECT o.order_id, (SELECT max(p.paid_amount) FROM hive.raw.payments p WHERE p.order_id = o.order_id) AS mp FROM hive.raw.orders o",
            &[("hive.raw.orders", &["order_id"]), ("hive.raw.payments", &["paid_amount"])],
        ),
        ("SELECT 1 AS x UNION SELECT o.amount FROM hive.raw.orders o", &[("hive.raw.orders", &["amount"])]),
    ];
    for (sql, expected) in cases {
        lineage(sql, expected);
    }
}

#[test]
fn wrapping_statements_match_bare_select() {
    let body = "SELECT u.user_name, o.amount FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id WHERE o.status = 'PAID'";
    let expected: &[(&str, &[&str])] = &[("hive.raw.orders", &["amount"]), ("hive.raw.users", &["user_name"])];
    lineage(body, expected);
    for prefix in [
        "CREATE VIEW hive.x.v AS ",
        "CREATE TABLE hive.x.t AS ",
        "INSERT INTO hive.x.t ",
    ] {
        lineage(&format!("{prefix}{body}"), expected);
    }
}

#[test]
fn unwrap_coverage() {
    let want: &[(&str, &[&str])] = &[("hive.raw.orders", &["amount"])];
    for (dialect, sql) in [
        (
            "trino",
            "CREATE VIEW hive.x.v AS SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "trino",
            "CREATE VIEW hive.x.v (a) AS SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "postgresql",
            "CREATE MATERIALIZED VIEW mv AS SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "trino",
            "CREATE TABLE hive.x.t AS WITH c AS (SELECT o.amount FROM hive.raw.orders o) SELECT amount FROM c",
        ),
        (
            "trino",
            "INSERT INTO hive.x.t WITH c AS (SELECT o.amount FROM hive.raw.orders o) SELECT amount FROM c",
        ),
        (
            "postgresql",
            "INSERT INTO t SELECT o.amount FROM hive.raw.orders o RETURNING *",
        ),
        (
            "spark",
            "INSERT OVERWRITE TABLE t SELECT o.amount FROM hive.raw.orders o",
        ),
        ("spark", "CACHE TABLE c AS SELECT o.amount FROM hive.raw.orders o"),
        ("trino", "EXPLAIN SELECT o.amount FROM hive.raw.orders o"),
    ] {
        let opts = trino_schema().dialect(common::dialect(dialect));
        check(sql, want, &opts);
    }
}

#[test]
fn non_query_and_dml_statements() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])])> = vec![
        ("trino", "INSERT INTO hive.x.t VALUES (1, 'a')", &[]),
        ("trino", "CREATE TABLE hive.x.t (id BIGINT, name VARCHAR)", &[]),
        ("trino", "CREATE TABLE hive.x.t (LIKE hive.raw.orders)", &[]),
        ("trino", "DROP TABLE hive.x.t", &[]),
        ("trino", "DELETE FROM hive.x.t WHERE id IN (SELECT o.user_id FROM hive.raw.orders o)", &[]),
        ("trino", "UPDATE hive.x.t SET amount = (SELECT max(o.amount) FROM hive.raw.orders o) WHERE id = 1", &[("hive.raw.orders", &["amount"]), ("hive.x.t", &[])]),
        ("trino", "MERGE INTO hive.x.t USING (SELECT o.user_id, o.amount FROM hive.raw.orders o) s ON t.id = s.user_id WHEN MATCHED THEN UPDATE SET amount = s.amount", &[("hive.raw.orders", &["amount"]), ("hive.x.t", &[])]),
        ("postgresql", "UPDATE t SET x = s.v FROM (SELECT o.user_id, o.amount AS v FROM hive.raw.orders o) s WHERE t.id = s.user_id", &[("hive.raw.orders", &["amount"]), ("t", &[])]),
        ("postgresql", "UPDATE t SET x = y + 1 WHERE id = 7", &[("t", &["y"])]),
        ("trino", "MERGE INTO t USING r ON t.id = r.id WHEN MATCHED AND r.flag = 1 THEN UPDATE SET x = r.v WHEN NOT MATCHED THEN INSERT (a) VALUES (r.b)", &[("r", &["b", "v"]), ("t", &[])]),
    ];
    for (dialect, sql, expected) in cases {
        let opts = trino_schema().dialect(common::dialect(dialect));
        check(sql, expected, &opts);
    }
}

#[test]
fn schema_handling() {
    check(
        "SELECT a, b FROM foo",
        &[("foo", &["a", "b"])],
        &options("trino").schema([("foo", vec!["a", "b"]), ("boo", vec!["b"])]),
    );
    check(
        "SELECT a, b FROM foo",
        &[("foo", &["a", "b"])],
        &options("trino").schema([("foo", vec![]), ("other_table", vec!["a", "b", "c"])]),
    );
    check(
        "SELECT brand, actual_end_time, actual_a_hcp_count, event_osmp_cd FROM vdm_rda.launch_to_engage.event",
        &[(
            "vdm_rda.launch_to_engage.event",
            &["actual_a_hcp_count", "actual_end_time", "brand", "event_osmp_cd"],
        )],
        &options("trino").schema([
            ("vdm_rda.launch_to_engage.event", vec![]),
            (
                "vdm_rda.launch_to_engage.event_attendance",
                vec!["actual_end_time", "brand", "bu"],
            ),
            (
                "vdm_rda_launch_to_engage.event1",
                vec!["brand", "actual_a_hcp_count", "event_nm"],
            ),
        ]),
    );
    let star = "SELECT * FROM hive.raw.orders";
    check(star, &[("hive.raw.orders", &[])], &options("trino"));
    check(
        star,
        &[("hive.raw.orders", &[])],
        &options("trino").schema([("hive.raw.users", vec!["user_id", "email"])]),
    );

    let sql = "SELECT amount FROM raw.orders o JOIN raw.users u ON o.id = u.id";
    let control = column_origins(sql, &options("trino").schema([("hive.zzz.unrelated", vec!["amount"])])).unwrap();
    let probe = column_origins(
        sql,
        &options("trino").schema([("hive.braw.orders", vec!["amount", "id"])]),
    )
    .unwrap();
    assert_eq!(control, probe);

    let got = column_origins("SELECT user_id, amount FROM hive.raw.orders", &trino_schema()).unwrap();
    assert_eq!(got.keys().collect::<Vec<_>>(), ["hive.raw.orders"]);

    check(
        "SELECT interaction_id FROM iceberg.rda_launch_to_engage.interaction UNION ALL SELECT interaction_id FROM hive.rda_launch_to_engage.interaction",
        &[("iceberg.rda_launch_to_engage.interaction", &["interaction_id"]), ("hive.rda_launch_to_engage.interaction", &["interaction_id"])],
        &options("trino"),
    );
}

#[test]
fn deterministic_without_schema() {
    let sql = "SELECT email FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id";
    let first = column_origins(sql, &options("trino")).unwrap();
    for _ in 0..4 {
        assert_eq!(column_origins(sql, &options("trino")).unwrap(), first);
    }
}

#[test]
fn errors() {
    assert_eq!(
        column_origins("SELECT a FROM t; SELECT b FROM r", &Options::new())
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(
        column_origins("SELECT FROM WHERE", &Options::new()).unwrap_err().kind(),
        ErrorKind::Parse
    );
}
