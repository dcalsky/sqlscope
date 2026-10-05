#![allow(clippy::type_complexity)]

mod common;

use common::*;
use sqlscope::{inject_ctes, CteDef, ErrorKind, Options};

fn check(dialect: &str, sql: &str, ctes: &[CteDef], want: &str) {
    let out = inject_ctes(sql, ctes, &options(dialect)).unwrap_or_else(|e| panic!("{sql}: {e}"));
    parse_one(&out, dialect);
    assert_eq!(out, want, "\n in: {sql}");
}

fn cte(name: &str, query: &str) -> CteDef {
    CteDef::new(name, query)
}

#[test]
fn basic() {
    check(
        "trino",
        "SELECT * FROM foo WHERE amount > 100",
        &[cte("foo", "SELECT order_id, amount FROM orders WHERE status = 'paid'")],
        "WITH foo AS (SELECT order_id, amount FROM orders WHERE status = 'paid') SELECT * FROM foo WHERE amount > 100",
    );
}

#[test]
fn join_shapes() {
    let cases: Vec<(&str, Vec<CteDef>, &str)> = vec![
        (
            "SELECT f.order_id, c.name FROM foo f JOIN customers c ON f.customer_id = c.id WHERE f.amount > 100",
            vec![cte("foo", "SELECT order_id, customer_id, amount FROM orders")],
            "WITH foo AS (SELECT order_id, customer_id, amount FROM orders) SELECT f.order_id, c.name FROM foo AS f JOIN customers AS c ON f.customer_id = c.id WHERE f.amount > 100",
        ),
        (
            "SELECT employee.name, manager.name AS manager_name FROM foo employee LEFT JOIN foo manager ON employee.manager_id = manager.employee_id",
            vec![cte("foo", "SELECT employee_id, manager_id, name FROM employees")],
            "WITH foo AS (SELECT employee_id, manager_id, name FROM employees) SELECT employee.name, manager.name AS manager_name FROM foo AS employee LEFT JOIN foo AS manager ON employee.manager_id = manager.employee_id",
        ),
        (
            "SELECT * FROM foo WHERE customer_name IS NOT NULL",
            vec![cte("foo", "SELECT o.order_id, c.name AS customer_name FROM orders o LEFT JOIN customers c ON o.customer_id = c.id")],
            "WITH foo AS (SELECT o.order_id, c.name AS customer_name FROM orders AS o LEFT JOIN customers AS c ON o.customer_id = c.id) SELECT * FROM foo WHERE customer_name IS NOT NULL",
        ),
        (
            "SELECT o.order_id, o.amount, p.paid_amount FROM orders_view o LEFT JOIN payments_view p ON o.order_id = p.order_id",
            vec![
                cte("orders_view", "SELECT order_id, amount FROM orders"),
                cte("payments_view", "SELECT order_id, paid_amount FROM payments"),
            ],
            "WITH orders_view AS (SELECT order_id, amount FROM orders), payments_view AS (SELECT order_id, paid_amount FROM payments) SELECT o.order_id, o.amount, p.paid_amount FROM orders_view AS o LEFT JOIN payments_view AS p ON o.order_id = p.order_id",
        ),
        (
            "SELECT * FROM enriched",
            vec![
                cte("foo", "SELECT id, dimension_id FROM source_table"),
                cte("enriched", "SELECT f.id, d.label FROM foo f JOIN dimensions d ON f.dimension_id = d.id"),
            ],
            "WITH foo AS (SELECT id, dimension_id FROM source_table), enriched AS (SELECT f.id, d.label FROM foo AS f JOIN dimensions AS d ON f.dimension_id = d.id) SELECT * FROM enriched",
        ),
        (
            "WITH matched AS (SELECT f.id, d.label FROM foo f CROSS JOIN dimensions d) SELECT * FROM matched",
            vec![cte("foo", "SELECT id FROM source_table")],
            "WITH foo AS (SELECT id FROM source_table), matched AS (SELECT f.id, d.label FROM foo AS f CROSS JOIN dimensions AS d) SELECT * FROM matched",
        ),
    ];
    for (sql, ctes, want) in cases {
        check("trino", sql, &ctes, want);
    }
}

#[test]
fn join_across_dialects() {
    for (dialect, want) in [
        ("mysql", "WITH foo AS (SELECT id, owner_id FROM source_table) SELECT f.id, u.name FROM foo AS f JOIN users AS u ON f.owner_id = u.id"),
        ("starrocks", "WITH foo AS (SELECT id, owner_id FROM source_table) SELECT f.id, u.name FROM foo f JOIN users u ON f.owner_id = u.id"),
        ("postgres", "WITH foo AS (SELECT id, owner_id FROM source_table) SELECT f.id, u.name FROM foo AS f JOIN users AS u ON f.owner_id = u.id"),
        ("trino", "WITH foo AS (SELECT id, owner_id FROM source_table) SELECT f.id, u.name FROM foo AS f JOIN users AS u ON f.owner_id = u.id"),
    ] {
        check(
            dialect,
            "SELECT f.id, u.name FROM foo f JOIN users u ON f.owner_id = u.id",
            &[cte("foo", "SELECT id, owner_id FROM source_table")],
            want,
        );
    }
}

#[test]
fn query_shapes() {
    let cases = [
        (
            "SELECT customer_id, SUM(amount) AS total_amount FROM foo GROUP BY customer_id HAVING SUM(amount) > 100 ORDER BY total_amount DESC LIMIT 10",
            cte("foo", "SELECT customer_id, amount FROM orders"),
            "WITH foo AS (SELECT customer_id, amount FROM orders) SELECT customer_id, SUM(amount) AS total_amount FROM foo GROUP BY customer_id HAVING SUM(amount) > 100 ORDER BY total_amount DESC LIMIT 10",
        ),
        (
            "SELECT id FROM foo WHERE id > 0",
            cte("foo", "SELECT id FROM live_records UNION ALL SELECT id FROM archived_records"),
            "WITH foo AS (SELECT id FROM live_records UNION ALL SELECT id FROM archived_records) SELECT id FROM foo WHERE id > 0",
        ),
        (
            "SELECT f.order_id FROM foo f WHERE EXISTS (SELECT 1 FROM refunds r WHERE r.order_id = f.order_id)",
            cte("foo", "SELECT order_id FROM orders"),
            "WITH foo AS (SELECT order_id FROM orders) SELECT f.order_id FROM foo AS f WHERE EXISTS(SELECT 1 FROM refunds AS r WHERE r.order_id = f.order_id)",
        ),
        (
            "SELECT q.id FROM (SELECT id FROM foo WHERE id > 0) q",
            cte("foo", "SELECT id FROM source_table"),
            "WITH foo AS (SELECT id FROM source_table) SELECT q.id FROM (SELECT id FROM foo WHERE id > 0) AS q",
        ),
        (
            "SELECT id FROM foo WHERE row_num = 1",
            cte("foo", "SELECT id, ROW_NUMBER() OVER (PARTITION BY owner_id ORDER BY created_at DESC) AS row_num FROM events"),
            "WITH foo AS (SELECT id, ROW_NUMBER() OVER (PARTITION BY owner_id ORDER BY created_at DESC) AS row_num FROM events) SELECT id FROM foo WHERE row_num = 1",
        ),
        (
            "SELECT id FROM foo",
            cte("foo", "SELECT id FROM foo WHERE active = TRUE"),
            "WITH foo AS (SELECT id FROM foo WHERE active = TRUE) SELECT id FROM foo",
        ),
        (
            "SELECT * FROM foo.bar WHERE total_amount > 1000",
            cte("bar", "WITH paid_orders AS (SELECT customer_id, amount FROM orders WHERE status = 'paid') SELECT customer_id, SUM(amount) AS total_amount FROM paid_orders GROUP BY customer_id"),
            "WITH bar AS (WITH paid_orders AS (SELECT customer_id, amount FROM orders WHERE status = 'paid') SELECT customer_id, SUM(amount) AS total_amount FROM paid_orders GROUP BY customer_id) SELECT * FROM foo.bar WHERE total_amount > 1000",
        ),
        (
            "WITH large_orders AS (SELECT order_id FROM foo WHERE amount > 100) SELECT * FROM large_orders",
            cte("foo", "SELECT order_id, amount FROM orders"),
            "WITH foo AS (SELECT order_id, amount FROM orders), large_orders AS (SELECT order_id FROM foo WHERE amount > 100) SELECT * FROM large_orders",
        ),
        (
            "WITH RECURSIVE seq AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM seq WHERE n < 3) SELECT foo.id, seq.n FROM foo CROSS JOIN seq",
            cte("foo", "SELECT id FROM source_table"),
            "WITH RECURSIVE foo AS (SELECT id FROM source_table), seq AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM seq WHERE n < 3) SELECT foo.id, seq.n FROM foo CROSS JOIN seq",
        ),
        (
            "SELECT id FROM foo UNION ALL SELECT id FROM foo",
            cte("foo", "SELECT id FROM source_table"),
            "WITH foo AS (SELECT id FROM source_table) SELECT id FROM foo UNION ALL SELECT id FROM foo",
        ),
        (
            "SELECT f.interaction_id, u.name FROM filtered f JOIN hive.rda_launch_to_engage.users u ON f.user_id = u.user_id",
            cte("filtered", "SELECT interaction_id, user_id FROM iceberg.rda_launch_to_engage.interaction WHERE active = TRUE"),
            "WITH filtered AS (SELECT interaction_id, user_id FROM iceberg.rda_launch_to_engage.interaction WHERE active = TRUE) SELECT f.interaction_id, u.name FROM filtered AS f JOIN hive.rda_launch_to_engage.users AS u ON f.user_id = u.user_id",
        ),
    ];
    for (sql, def, want) in cases {
        check("trino", sql, &[def], want);
    }
}

#[test]
fn preserves_definition_with_clause_in_every_dialect() {
    for dialect in ["mysql", "starrocks", "postgres", "trino"] {
        check(
            dialect,
            "SELECT * FROM foo",
            &[cte("foo", "WITH paid AS (SELECT order_id, amount FROM orders WHERE status = 'paid') SELECT order_id, amount FROM paid")],
            "WITH foo AS (WITH paid AS (SELECT order_id, amount FROM orders WHERE status = 'paid') SELECT order_id, amount FROM paid) SELECT * FROM foo",
        );
        check(
            dialect,
            "SELECT * FROM foo",
            &[cte("foo", "SELECT id FROM source_table")],
            "WITH foo AS (SELECT id FROM source_table) SELECT * FROM foo",
        );
    }
}

#[test]
fn keeps_definition_order() {
    check(
        "trino",
        "SELECT * FROM bar",
        &[
            cte("foo", "SELECT id FROM source_table"),
            cte("bar", "SELECT id FROM foo WHERE id > 0"),
        ],
        "WITH foo AS (SELECT id FROM source_table), bar AS (SELECT id FROM foo WHERE id > 0) SELECT * FROM bar",
    );
}

#[test]
fn composition_is_structural() {
    check(
        "postgres",
        "SELECT * FROM foo",
        &[cte(
            "foo",
            "SELECT '); DROP TABLE secret; --' AS txt FROM source_table -- trailing comment",
        )],
        "WITH foo AS (SELECT '); DROP TABLE secret; --' AS txt FROM source_table) SELECT * FROM foo",
    );
    check(
        "postgres",
        r#"SELECT * FROM "daily orders""#,
        &[cte("daily orders", "SELECT id FROM orders")],
        r#"WITH "daily orders" AS (SELECT id FROM orders) SELECT * FROM "daily orders""#,
    );
    check(
        "postgres",
        r#"SELECT * FROM "Foo""#,
        &[cte("Foo", "SELECT id FROM orders")],
        r#"WITH "Foo" AS (SELECT id FROM orders) SELECT * FROM "Foo""#,
    );
}

#[test]
fn empty_definitions_are_a_noop() {
    assert_eq!(
        inject_ctes("  not even SQL  ", &[], &Options::new()).unwrap(),
        "  not even SQL  "
    );
}

#[test]
fn invalid_definitions_are_rejected() {
    let cases: Vec<(&str, Vec<CteDef>, &str)> = vec![
        ("SELECT * FROM foo", vec![cte("", "SELECT 1")], "name must not be empty"),
        (
            "SELECT * FROM foo",
            vec![cte("foo", "")],
            r#"CTE "foo" query must not be empty"#,
        ),
        (
            "SELECT * FROM foo",
            vec![cte("foo", "SELECT 1"), cte("FOO", "SELECT 2")],
            r#"duplicate CTE "FOO""#,
        ),
        (
            "WITH foo AS (SELECT 1) SELECT * FROM foo",
            vec![cte("foo", "SELECT 2")],
            r#"CTE "foo" conflicts with existing CTE"#,
        ),
    ];
    for (sql, ctes, message) in cases {
        let err = inject_ctes(sql, &ctes, &Options::new()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidArgument, "{err}");
        assert!(err.to_string().contains(message), "{err} lacks {message}");
    }
}

#[test]
fn sql_failures_are_classified() {
    let big = "x".repeat(1 << 20);
    let cases: Vec<(&str, Vec<CteDef>, ErrorKind)> = vec![
        ("SELECT FROM", vec![cte("foo", "SELECT 1")], ErrorKind::Parse),
        ("SELECT * FROM foo", vec![cte("foo", "SELECT FROM")], ErrorKind::Parse),
        (
            "SELECT * FROM foo; SELECT 2",
            vec![cte("foo", "SELECT 1")],
            ErrorKind::Unsupported,
        ),
        (
            "SELECT * FROM foo",
            vec![cte("foo", "SELECT 1; SELECT 2")],
            ErrorKind::Unsupported,
        ),
        ("DELETE FROM foo", vec![cte("foo", "SELECT 1")], ErrorKind::Unsupported),
        (
            "SELECT * FROM foo",
            vec![cte("foo", "DELETE FROM source_table")],
            ErrorKind::Unsupported,
        ),
        ("SELECT * FROM foo", vec![cte("foo", &big)], ErrorKind::Unsupported),
    ];
    for (sql, ctes, kind) in cases {
        let err = inject_ctes(sql, &ctes, &Options::new()).unwrap_err();
        assert_eq!(err.kind(), kind, "{sql}: {err}");
    }
}
