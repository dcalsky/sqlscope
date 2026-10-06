//! Ported from sql-guard `bind_ctes_test.go` (BindCTEs -> inject_ctes).

use sqlscope::{inject_ctes, CteDef, ErrorKind};

use crate::common::*;

fn bind(dialect: &str, sql: &str, ctes: &[CteDef]) -> String {
    let out = inject_ctes(sql, ctes, &options(dialect)).unwrap_or_else(|e| panic!("inject_ctes({sql:?}): {e}"));
    parse_one(&out, dialect);
    out
}

fn cte(name: &str, query: &str) -> CteDef {
    CteDef::new(name, query)
}

#[test]
fn bind_ctes_basic() {
    let out = bind(
        "trino",
        "SELECT * FROM foo WHERE amount > 100",
        &[cte("foo", "SELECT order_id, amount FROM orders WHERE status = 'paid'")],
    );
    assert_eq!(
        out,
        "WITH foo AS (SELECT order_id, amount FROM orders WHERE status = 'paid') SELECT * FROM foo WHERE amount > 100"
    );
}

#[test]
fn bind_ctes_join_queries() {
    let cases: Vec<(&str, &str, Vec<CteDef>, &str)> = vec![
        (
            "consumer joins binding with physical table",
            "SELECT f.order_id, c.name
				FROM foo f
				JOIN customers c ON f.customer_id = c.id
				WHERE f.amount > 100",
            vec![cte("foo", "SELECT order_id, customer_id, amount FROM orders")],
            "WITH foo AS (SELECT order_id, customer_id, amount FROM orders) SELECT f.order_id, c.name FROM foo AS f JOIN customers AS c ON f.customer_id = c.id WHERE f.amount > 100",
        ),
        (
            "consumer self joins binding",
            "SELECT employee.name, manager.name AS manager_name
				FROM foo employee
				LEFT JOIN foo manager ON employee.manager_id = manager.employee_id",
            vec![cte("foo", "SELECT employee_id, manager_id, name FROM employees")],
            "WITH foo AS (SELECT employee_id, manager_id, name FROM employees) SELECT employee.name, manager.name AS manager_name FROM foo AS employee LEFT JOIN foo AS manager ON employee.manager_id = manager.employee_id",
        ),
        (
            "binding query contains join",
            "SELECT * FROM foo WHERE customer_name IS NOT NULL",
            vec![cte(
                "foo",
                "SELECT o.order_id, c.name AS customer_name
					FROM orders o
					LEFT JOIN customers c ON o.customer_id = c.id",
            )],
            "WITH foo AS (SELECT o.order_id, c.name AS customer_name FROM orders AS o LEFT JOIN customers AS c ON o.customer_id = c.id) SELECT * FROM foo WHERE customer_name IS NOT NULL",
        ),
        (
            "consumer joins two bindings",
            "SELECT o.order_id, o.amount, p.paid_amount
				FROM orders_view o
				LEFT JOIN payments_view p ON o.order_id = p.order_id",
            vec![
                cte("orders_view", "SELECT order_id, amount FROM orders"),
                cte("payments_view", "SELECT order_id, paid_amount FROM payments"),
            ],
            "WITH orders_view AS (SELECT order_id, amount FROM orders), payments_view AS (SELECT order_id, paid_amount FROM payments) SELECT o.order_id, o.amount, p.paid_amount FROM orders_view AS o LEFT JOIN payments_view AS p ON o.order_id = p.order_id",
        ),
        (
            "dependent binding joins earlier binding",
            "SELECT * FROM enriched",
            vec![
                cte("foo", "SELECT id, dimension_id FROM source_table"),
                cte(
                    "enriched",
                    "SELECT f.id, d.label
						FROM foo f
						JOIN dimensions d ON f.dimension_id = d.id",
                ),
            ],
            "WITH foo AS (SELECT id, dimension_id FROM source_table), enriched AS (SELECT f.id, d.label FROM foo AS f JOIN dimensions AS d ON f.dimension_id = d.id) SELECT * FROM enriched",
        ),
        (
            "join lives in existing consumer CTE",
            "WITH matched AS (
					SELECT f.id, d.label
					FROM foo f
					CROSS JOIN dimensions d
				)
				SELECT * FROM matched",
            vec![cte("foo", "SELECT id FROM source_table")],
            "WITH foo AS (SELECT id FROM source_table), matched AS (SELECT f.id, d.label FROM foo AS f CROSS JOIN dimensions AS d) SELECT * FROM matched",
        ),
    ];
    for (name, sql, ctes, want) in cases {
        assert_eq!(bind("trino", sql, &ctes), want, "{name}");
    }
}

#[test]
fn bind_ctes_join_across_dialects() {
    for (dialect, want) in [
        ("mysql", "WITH foo AS (SELECT id, owner_id FROM source_table) SELECT f.id, u.name FROM foo AS f JOIN users AS u ON f.owner_id = u.id"),
        ("starrocks", "WITH foo AS (SELECT id, owner_id FROM source_table) SELECT f.id, u.name FROM foo f JOIN users u ON f.owner_id = u.id"),
        ("postgres", "WITH foo AS (SELECT id, owner_id FROM source_table) SELECT f.id, u.name FROM foo AS f JOIN users AS u ON f.owner_id = u.id"),
        ("trino", "WITH foo AS (SELECT id, owner_id FROM source_table) SELECT f.id, u.name FROM foo AS f JOIN users AS u ON f.owner_id = u.id"),
    ] {
        let out = bind(
            dialect,
            "SELECT f.id, u.name FROM foo f JOIN users u ON f.owner_id = u.id",
            &[cte("foo", "SELECT id, owner_id FROM source_table")],
        );
        assert_eq!(out, want, "{dialect}");
    }
}

#[test]
fn bind_ctes_additional_query_shapes() {
    let cases = [
        (
            "aggregate having order and limit",
            "SELECT customer_id, SUM(amount) AS total_amount
				FROM foo
				GROUP BY customer_id
				HAVING SUM(amount) > 100
				ORDER BY total_amount DESC
				LIMIT 10",
            cte("foo", "SELECT customer_id, amount FROM orders"),
            "WITH foo AS (SELECT customer_id, amount FROM orders) SELECT customer_id, SUM(amount) AS total_amount FROM foo GROUP BY customer_id HAVING SUM(amount) > 100 ORDER BY total_amount DESC LIMIT 10",
        ),
        (
            "binding is set operation",
            "SELECT id FROM foo WHERE id > 0",
            cte("foo", "SELECT id FROM live_records UNION ALL SELECT id FROM archived_records"),
            "WITH foo AS (SELECT id FROM live_records UNION ALL SELECT id FROM archived_records) SELECT id FROM foo WHERE id > 0",
        ),
        (
            "correlated exists references binding",
            "SELECT f.order_id
				FROM foo f
				WHERE EXISTS (
					SELECT 1 FROM refunds r WHERE r.order_id = f.order_id
				)",
            cte("foo", "SELECT order_id FROM orders"),
            "WITH foo AS (SELECT order_id FROM orders) SELECT f.order_id FROM foo AS f WHERE EXISTS(SELECT 1 FROM refunds AS r WHERE r.order_id = f.order_id)",
        ),
        (
            "binding inside derived table",
            "SELECT q.id FROM (SELECT id FROM foo WHERE id > 0) q",
            cte("foo", "SELECT id FROM source_table"),
            "WITH foo AS (SELECT id FROM source_table) SELECT q.id FROM (SELECT id FROM foo WHERE id > 0) AS q",
        ),
        (
            "window function in binding",
            "SELECT id FROM foo WHERE row_num = 1",
            cte(
                "foo",
                "SELECT id,
					ROW_NUMBER() OVER (PARTITION BY owner_id ORDER BY created_at DESC) AS row_num
					FROM events",
            ),
            "WITH foo AS (SELECT id, ROW_NUMBER() OVER (PARTITION BY owner_id ORDER BY created_at DESC) AS row_num FROM events) SELECT id FROM foo WHERE row_num = 1",
        ),
        (
            "binding may read same named physical table",
            "SELECT id FROM foo",
            cte("foo", "SELECT id FROM foo WHERE active = TRUE"),
            "WITH foo AS (SELECT id FROM foo WHERE active = TRUE) SELECT id FROM foo",
        ),
    ];
    for (name, sql, definition, want) in cases {
        assert_eq!(bind("trino", sql, &[definition]), want, "{name}");
    }
}

#[test]
fn bind_ctes_does_not_shadow_qualified_physical_table() {
    let out = bind(
        "trino",
        "SELECT * FROM foo.bar WHERE total_amount > 1000",
        &[cte(
            "bar",
            "
				WITH paid_orders AS (
					SELECT customer_id, amount
					FROM orders
					WHERE status = 'paid'
				)
				SELECT customer_id, SUM(amount) AS total_amount
				FROM paid_orders
				GROUP BY customer_id",
        )],
    );
    assert_eq!(
        out,
        "WITH bar AS (WITH paid_orders AS (SELECT customer_id, amount FROM orders WHERE status = 'paid') SELECT customer_id, SUM(amount) AS total_amount FROM paid_orders GROUP BY customer_id) SELECT * FROM foo.bar WHERE total_amount > 1000"
    );
}

#[test]
fn bind_ctes_preserves_source_with_clause() {
    for dialect in ["mysql", "starrocks", "postgres", "trino"] {
        let out = bind(
            dialect,
            "SELECT * FROM foo",
            &[cte(
                "foo",
                "WITH paid AS (
						SELECT order_id, amount FROM orders WHERE status = 'paid'
					)
					SELECT order_id, amount FROM paid",
            )],
        );
        assert_eq!(
            out,
            "WITH foo AS (WITH paid AS (SELECT order_id, amount FROM orders WHERE status = 'paid') SELECT order_id, amount FROM paid) SELECT * FROM foo",
            "{dialect}"
        );
    }
}

#[test]
fn bind_ctes_prepends_bindings_to_consumer_with_clause() {
    let out = bind(
        "trino",
        "WITH large_orders AS (
			SELECT order_id FROM foo WHERE amount > 100
		)
		SELECT * FROM large_orders",
        &[cte("foo", "SELECT order_id, amount FROM orders")],
    );
    assert_eq!(
        out,
        "WITH foo AS (SELECT order_id, amount FROM orders), large_orders AS (SELECT order_id FROM foo WHERE amount > 100) SELECT * FROM large_orders"
    );
}

#[test]
fn bind_ctes_preserves_recursive_consumer_with_clause() {
    let out = bind(
        "trino",
        "WITH RECURSIVE seq AS (
			SELECT 1 AS n
			UNION ALL
			SELECT n + 1 FROM seq WHERE n < 3
		)
		SELECT foo.id, seq.n FROM foo CROSS JOIN seq",
        &[cte("foo", "SELECT id FROM source_table")],
    );
    assert_eq!(
        out,
        "WITH RECURSIVE foo AS (SELECT id FROM source_table), seq AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM seq WHERE n < 3) SELECT foo.id, seq.n FROM foo CROSS JOIN seq"
    );
}

#[test]
fn bind_ctes_keeps_binding_order() {
    let out = bind(
        "trino",
        "SELECT * FROM bar",
        &[
            cte("foo", "SELECT id FROM source_table"),
            cte("bar", "SELECT id FROM foo WHERE id > 0"),
        ],
    );
    assert_eq!(
        out,
        "WITH foo AS (SELECT id FROM source_table), bar AS (SELECT id FROM foo WHERE id > 0) SELECT * FROM bar"
    );
}

#[test]
fn bind_ctes_handles_set_operation_consumer() {
    let out = bind(
        "trino",
        "SELECT id FROM foo UNION ALL SELECT id FROM foo",
        &[cte("foo", "SELECT id FROM source_table")],
    );
    assert_eq!(
        out,
        "WITH foo AS (SELECT id FROM source_table) SELECT id FROM foo UNION ALL SELECT id FROM foo"
    );
}

#[test]
fn bind_ctes_does_not_interpolate_query_text() {
    let out = bind(
        "postgres",
        "SELECT * FROM foo",
        &[cte(
            "foo",
            "SELECT '); DROP TABLE secret; --' AS txt FROM source_table -- trailing comment",
        )],
    );
    assert_eq!(
        out,
        "WITH foo AS (SELECT '); DROP TABLE secret; --' AS txt FROM source_table) SELECT * FROM foo"
    );
}

#[test]
fn bind_ctes_quotes_binding_name() {
    let out = bind(
        "postgres",
        r#"SELECT * FROM "daily orders""#,
        &[cte("daily orders", "SELECT id FROM orders")],
    );
    assert_eq!(
        out,
        r#"WITH "daily orders" AS (SELECT id FROM orders) SELECT * FROM "daily orders""#
    );
}

#[test]
fn bind_ctes_preserves_mixed_case_binding_name() {
    let out = bind(
        "postgres",
        r#"SELECT * FROM "Foo""#,
        &[cte("Foo", "SELECT id FROM orders")],
    );
    assert_eq!(out, r#"WITH "Foo" AS (SELECT id FROM orders) SELECT * FROM "Foo""#);
}

#[test]
fn bind_ctes_supports_all_rewrite_dialects() {
    for dialect in ["mysql", "starrocks", "postgres", "trino"] {
        let out = bind(
            dialect,
            "SELECT * FROM foo",
            &[cte("foo", "SELECT id FROM source_table")],
        );
        assert_eq!(
            out, "WITH foo AS (SELECT id FROM source_table) SELECT * FROM foo",
            "{dialect}"
        );
    }
}

#[test]
fn bind_ctes_no_bindings_preserves_input() {
    // sql-guard also ignored an invalid dialect here; in sqlscope the dialect
    // is a parsed value, so there is no invalid one to pass.
    let input = "  not even SQL  ";
    assert_eq!(inject_ctes(input, &[], &options("trino")).unwrap(), input);
}

#[test]
fn bind_ctes_rejects_invalid_configuration() {
    // Same failures as sql-guard; the messages are sqlscope's own.
    let cases: Vec<(&str, &str, Vec<CteDef>, &str)> = vec![
        (
            "empty name",
            "SELECT * FROM foo",
            vec![cte("", "SELECT 1")],
            "name must not be empty",
        ),
        (
            "empty query",
            "SELECT * FROM foo",
            vec![cte("foo", "")],
            "CTE \"foo\" query must not be empty",
        ),
        (
            "duplicate binding",
            "SELECT * FROM foo",
            vec![cte("foo", "SELECT 1"), cte("FOO", "SELECT 2")],
            "duplicate CTE \"FOO\"",
        ),
        (
            "consumer collision",
            "WITH foo AS (SELECT 1) SELECT * FROM foo",
            vec![cte("foo", "SELECT 2")],
            "CTE \"foo\" conflicts with existing CTE",
        ),
    ];
    for (name, sql, ctes, contains) in cases {
        let error = inject_ctes(sql, &ctes, &options("trino")).unwrap_err();
        assert!(error.message().contains(contains), "{name}: {error}");
        assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{name}: {error}");
    }
    let error = "oracle-ish".parse::<sqlscope::Dialect>().unwrap_err();
    assert!(error.message().contains("unknown dialect \"oracle-ish\""), "{error}");
}

#[test]
fn bind_ctes_classifies_sql_failures() {
    let huge = "x".repeat(1 << 20);
    let cases: Vec<(&str, &str, Vec<CteDef>, ErrorKind)> = vec![
        (
            "invalid consumer",
            "SELECT FROM",
            vec![cte("foo", "SELECT 1")],
            ErrorKind::Parse,
        ),
        (
            "invalid binding query",
            "SELECT * FROM foo",
            vec![cte("foo", "SELECT FROM")],
            ErrorKind::Parse,
        ),
        (
            "multiple consumer statements",
            "SELECT * FROM foo; SELECT 2",
            vec![cte("foo", "SELECT 1")],
            ErrorKind::Unsupported,
        ),
        (
            "multiple binding statements",
            "SELECT * FROM foo",
            vec![cte("foo", "SELECT 1; SELECT 2")],
            ErrorKind::Unsupported,
        ),
        (
            "non-query consumer",
            "DELETE FROM foo",
            vec![cte("foo", "SELECT 1")],
            ErrorKind::Unsupported,
        ),
        (
            "non-query binding",
            "SELECT * FROM foo",
            vec![cte("foo", "DELETE FROM source_table")],
            ErrorKind::Unsupported,
        ),
        (
            "combined input too large",
            "SELECT * FROM foo",
            vec![cte("foo", &huge)],
            ErrorKind::Unsupported,
        ),
    ];
    for (name, sql, ctes, kind) in cases {
        let error = inject_ctes(sql, &ctes, &options("trino")).unwrap_err();
        assert_eq!(error.kind(), kind, "{name}: {error}");
    }
}
