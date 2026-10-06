//! Ported from sql-guard `lineage_test.go` and `lineage_innerquery_test.go`
//! (LineageSourceColumns -> column_origins).
//!
//! sql-guard also had `LineageSourceColumnsConcurrent`, a parallel driver
//! whose result had to equal the serial one, and provenance-only producer /
//! namespace options; sqlscope has a single entry point and neither option, so
//! those comparisons reduce to the serial assertions. The inner-query tests
//! probed the engine through the Go client and are ported as lineage
//! assertions. `lineage_diff_test.go` dumped results for an external diff and
//! has nothing to assert.

use std::collections::BTreeMap;

use sqlscope::{column_origins, ErrorKind, Options};

use crate::common::*;

type Lineage = BTreeMap<String, Vec<String>>;

fn metadata() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("hive.raw.users", vec!["user_id", "user_name", "email"]),
        (
            "hive.raw.orders",
            vec![
                "order_id",
                "user_id",
                "amount",
                "quantity",
                "order_ts",
                "order_date",
                "status",
            ],
        ),
        ("hive.raw.payments", vec!["order_id", "paid_amount", "paid_at"]),
    ]
}

fn trino() -> Options {
    options("trino").schema(metadata())
}

fn lineage(sql: &str, opts: &Options) -> Lineage {
    column_origins(sql, opts).unwrap_or_else(|e| panic!("column_origins({sql:?}): {e}"))
}

fn assert_lineage(name: &str, sql: &str, expected: &[(&str, &[&str])]) {
    assert_eq!(lineage(sql, &trino()), map(expected), "{name}\nsql: {sql}");
}

#[test]
fn lineage_source_columns_rejects_multiple_statements() {
    let error = column_origins("SELECT a FROM t; SELECT secret FROM restricted", &options("trino")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
}

#[test]
fn trino_catalog_schema_table_with_wildcard_and_where() {
    assert_lineage(
        "trino_catalog_schema_table_with_wildcard_and_where",
        "
        CREATE VIEW hive.analytics.v_user_orders AS
        SELECT u.user_name, o.*
        FROM hive.raw.users u
        JOIN hive.raw.orders o ON u.user_id = o.user_id
        WHERE o.status = 'PAID'
          AND u.email IS NOT NULL
        ",
        &[
            (
                "hive.raw.orders",
                &[
                    "amount",
                    "order_date",
                    "order_id",
                    "order_ts",
                    "quantity",
                    "status",
                    "user_id",
                ],
            ),
            ("hive.raw.users", &["user_name"]),
        ],
    );
}

#[test]
fn trino_duplicate_column_with_qualified_sources_and_where() {
    assert_lineage(
        "trino_duplicate_column_with_qualified_sources_and_where",
        "
        CREATE VIEW hive.analytics.v_user_ids AS
        SELECT
            u.user_id AS user_id_from_users,
            o.user_id AS user_id_from_orders
        FROM hive.raw.users u
        JOIN hive.raw.orders o ON u.user_id = o.user_id
        WHERE o.status IN ('PAID', 'SHIPPED')
          AND u.email LIKE '%@example.com'
        ",
        &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])],
    );
}

#[test]
fn trino_ambiguous_bare_duplicate_column_with_where() {
    assert_lineage(
        "trino_ambiguous_bare_duplicate_column_with_where",
        "
        CREATE VIEW hive.analytics.v_ambiguous_user_id AS
        SELECT user_id
        FROM hive.raw.users u
        JOIN hive.raw.orders o ON u.user_id = o.user_id
        WHERE o.order_date >= DATE '2024-01-01'
        ",
        &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])],
    );
}

#[test]
fn trino_expression_and_function_columns_with_where() {
    assert_lineage(
        "trino_expression_and_function_columns_with_where",
        "
        CREATE VIEW hive.analytics.v_order_metrics AS
        SELECT
            o.amount * o.quantity AS gross_amount,
            date_trunc('day', o.order_ts) AS order_day,
            u.user_name
        FROM hive.raw.users u
        JOIN hive.raw.orders o ON u.user_id = o.user_id
        WHERE o.status = 'PAID'
          AND o.order_date >= DATE '2024-01-01'
        ",
        &[
            ("hive.raw.orders", &["amount", "order_ts", "quantity"]),
            ("hive.raw.users", &["user_name"]),
        ],
    );
}

#[test]
fn trino_cte_resolves_to_root_source_tables_with_where() {
    assert_lineage(
        "trino_cte_resolves_to_root_source_tables_with_where",
        "
        CREATE VIEW hive.analytics.v_paid_orders AS
        WITH paid_orders AS (
            SELECT
                o.order_id,
                o.user_id,
                p.paid_amount
            FROM hive.raw.orders o
            JOIN hive.raw.payments p ON o.order_id = p.order_id
            WHERE o.status = 'PAID'
              AND p.paid_at >= TIMESTAMP '2024-01-01 00:00:00'
        )
        SELECT
            po.order_id,
            po.user_id,
            po.paid_amount
        FROM paid_orders po
        WHERE po.paid_amount > 0
        ",
        &[
            ("hive.raw.orders", &["order_id", "user_id"]),
            ("hive.raw.payments", &["paid_amount"]),
        ],
    );
}

#[test]
fn lineage_across_statement_types() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])])> = vec![
        (
            "plain_select_explicit_columns",
            "SELECT user_id, user_name FROM hive.raw.users",
            &[("hive.raw.users", &["user_id", "user_name"])],
        ),
        (
            "plain_select_wildcard_expands_via_metadata",
            "SELECT * FROM hive.raw.orders",
            &[(
                "hive.raw.orders",
                &[
                    "amount",
                    "order_date",
                    "order_id",
                    "order_ts",
                    "quantity",
                    "status",
                    "user_id",
                ],
            )],
        ),
        (
            "plain_select_where_only_column_excluded",
            "SELECT order_id FROM hive.raw.orders WHERE status = 'X'",
            &[("hive.raw.orders", &["order_id"])],
        ),
        (
            "plain_select_join_filter_only_side_is_empty",
            "SELECT u.user_name
                  FROM hive.raw.users u
                  JOIN hive.raw.orders o ON u.user_id = o.user_id",
            &[("hive.raw.orders", &[]), ("hive.raw.users", &["user_name"])],
        ),
        (
            "union_merges_both_branches",
            "SELECT user_id FROM hive.raw.users
                  UNION
                  SELECT user_id FROM hive.raw.orders",
            &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])],
        ),
        (
            "plain_cte_resolves_to_root_table",
            "WITH a AS (SELECT o.user_id, o.amount FROM hive.raw.orders o),
                       b AS (SELECT a.user_id FROM a)
                  SELECT b.user_id FROM b",
            &[("hive.raw.orders", &["user_id"])],
        ),
        (
            "plain_subquery_resolves_to_root_table",
            "SELECT x.uid FROM (SELECT o.user_id AS uid FROM hive.raw.orders o) x",
            &[("hive.raw.orders", &["user_id"])],
        ),
        (
            "plain_select_window_partition_and_order_flow",
            "SELECT row_number() OVER (PARTITION BY o.user_id ORDER BY o.order_ts) AS rn
                  FROM hive.raw.orders o",
            &[("hive.raw.orders", &["order_ts", "user_id"])],
        ),
        (
            "create_table_as_select_with_where_excluded",
            "CREATE TABLE hive.x.t AS
                  SELECT o.amount, o.user_id FROM hive.raw.orders o WHERE o.status = 'X'",
            &[("hive.raw.orders", &["amount", "user_id"])],
        ),
        (
            "insert_into_select_with_where_excluded",
            "INSERT INTO hive.x.t SELECT o.amount FROM hive.raw.orders o WHERE o.status = 'X'",
            &[("hive.raw.orders", &["amount"])],
        ),
        (
            "create_view_over_union",
            "CREATE VIEW hive.x.v AS
                  SELECT u.user_id FROM hive.raw.users u
                  UNION
                  SELECT o.user_id FROM hive.raw.orders o",
            &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])],
        ),
    ];
    for (name, sql, expected) in cases {
        assert_lineage(name, sql, expected);
    }
}

#[test]
fn lineage_set_operation_value_and_filter_semantics() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])])> = vec![
        (
            "union_values_from_both_branches",
            "SELECT user_id FROM hive.raw.users
				UNION
				SELECT user_id FROM hive.raw.orders",
            &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])],
        ),
        (
            "union_all_values_from_both_branches",
            "SELECT user_id FROM hive.raw.users
				UNION ALL
				SELECT user_id FROM hive.raw.orders",
            &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])],
        ),
        (
            "except_right_branch_is_filter_only",
            "SELECT user_id FROM hive.raw.users
				EXCEPT
				SELECT user_id FROM hive.raw.orders",
            &[("hive.raw.orders", &[]), ("hive.raw.users", &["user_id"])],
        ),
        (
            "intersect_right_branch_is_filter_only",
            "SELECT user_id FROM hive.raw.users
				INTERSECT
				SELECT user_id FROM hive.raw.orders",
            &[("hive.raw.orders", &[]), ("hive.raw.users", &["user_id"])],
        ),
        (
            "shared_source_with_direct_and_filter_transformations_flows",
            "SELECT user_id FROM hive.raw.users
				EXCEPT
				SELECT user_id FROM hive.raw.users",
            &[("hive.raw.users", &["user_id"])],
        ),
        (
            "parenthesized_nested_union_keeps_every_value_branch",
            "SELECT user_id FROM hive.raw.users
				UNION
				(SELECT user_id FROM hive.raw.orders
				 UNION ALL
				 SELECT order_id FROM hive.raw.payments)",
            &[
                ("hive.raw.orders", &["user_id"]),
                ("hive.raw.payments", &["order_id"]),
                ("hive.raw.users", &["user_id"]),
            ],
        ),
        (
            "root_cte_referenced_by_each_union_branch",
            "WITH candidates AS (SELECT user_id FROM hive.raw.users)
				SELECT user_id FROM candidates
				UNION ALL
				SELECT user_id FROM candidates",
            &[("hive.raw.users", &["user_id"])],
        ),
    ];
    for (name, sql, expected) in cases {
        assert_lineage(name, sql, expected);
    }
}

#[test]
fn lineage_dml_assignment_value_flow() {
    let cases: Vec<(&str, &str, &str, &[(&str, &[&str])])> = vec![
        (
            "update_target_value_excludes_where_filter",
            "postgres",
            "UPDATE t SET x = y + 1 WHERE id = 7",
            &[("t", &["y"])],
        ),
        (
            "merge_update_and_insert_values_exclude_match_filters",
            "trino",
            "MERGE INTO t USING r ON t.id = r.id
				WHEN MATCHED AND r.flag = 1 THEN UPDATE SET x = r.v
				WHEN NOT MATCHED THEN INSERT (a) VALUES (r.b)",
            &[("r", &["b", "v"]), ("t", &[])],
        ),
    ];
    for (name, dialect, sql, expected) in cases {
        assert_eq!(lineage(sql, &options(dialect)), map(expected), "{name}");
    }
}

#[test]
fn lineage_plain_select_matches_equivalent_create_view() {
    let body = "SELECT u.user_name, o.amount
                  FROM hive.raw.users u
                  JOIN hive.raw.orders o ON u.user_id = o.user_id
                  WHERE o.status = 'PAID'";
    let plain = lineage(body, &trino());
    let view = lineage(&format!("CREATE VIEW hive.x.v AS {body}"), &trino());
    assert_eq!(plain, view);
    assert_eq!(
        plain,
        map(&[("hive.raw.orders", &["amount"]), ("hive.raw.users", &["user_name"])])
    );
}

#[test]
fn lineage_simple_select_from_foo() {
    let opts = options("trino").schema([("foo", vec!["a", "b"]), ("boo", vec!["b"])]);
    assert_eq!(lineage("SELECT a, b FROM foo", &opts), map(&[("foo", &["a", "b"])]));
}

#[test]
fn lineage_empty_source_metadata_ignores_unrelated_catalog_tables() {
    let opts = options("trino").schema([("foo", vec![]), ("other_table", vec!["a", "b", "c"])]);
    assert_eq!(lineage("SELECT a, b FROM foo", &opts), map(&[("foo", &["a", "b"])]));
}

#[test]
fn lineage_empty_source_metadata_credits_only_queried_table() {
    let opts = options("trino").schema([
        ("vdm_rda.launch_to_engage.event", vec![]),
        (
            "vdm_rda.launch_to_engage.event_attendance",
            vec!["actual_end_time", "brand", "bu"],
        ),
        (
            "vdm_rda_launch_to_engage.event1",
            vec!["brand", "actual_a_hcp_count", "event_nm"],
        ),
    ]);
    let got = lineage(
        "SELECT brand, actual_end_time, actual_a_hcp_count, event_osmp_cd
		 FROM vdm_rda.launch_to_engage.event",
        &opts,
    );
    assert_eq!(
        got,
        map(&[(
            "vdm_rda.launch_to_engage.event",
            &["actual_a_hcp_count", "actual_end_time", "brand", "event_osmp_cd"],
        )])
    );
}

#[test]
fn lineage_result_keys_are_query_source_tables_only() {
    let opts = options("trino").schema([
        ("hive.raw.users", vec!["user_id", "user_name"]),
        ("hive.raw.orders", vec!["order_id", "user_id", "amount"]),
        ("hive.raw.payments", vec!["order_id", "paid_amount"]),
    ]);
    let got = lineage("SELECT user_id, amount FROM hive.raw.orders", &opts);
    assert!(got.keys().all(|table| table == "hive.raw.orders"), "{got:?}");
}

#[test]
fn bughunt_filter_only_and_flow_semantics() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])])> = vec![
        (
            "order_by_only_column_excluded",
            "SELECT order_id FROM hive.raw.orders ORDER BY order_ts",
            &[("hive.raw.orders", &["order_id"])],
        ),
        (
            "having_only_column_excluded",
            "SELECT o.user_id, count(o.order_id) AS c FROM hive.raw.orders o
			 GROUP BY o.user_id HAVING max(o.amount) > 10",
            &[("hive.raw.orders", &["order_id", "user_id"])],
        ),
        (
            "column_in_projection_and_filter_flows",
            "SELECT status FROM hive.raw.orders WHERE status = 'X'",
            &[("hive.raw.orders", &["status"])],
        ),
        (
            "case_condition_and_branches_flow",
            "SELECT CASE WHEN status = 'X' THEN amount ELSE quantity END AS v FROM hive.raw.orders",
            &[("hive.raw.orders", &["amount", "quantity", "status"])],
        ),
        (
            "aggregate_argument_flows",
            "SELECT sum(o.amount) AS total FROM hive.raw.orders o GROUP BY o.status",
            &[("hive.raw.orders", &["amount"])],
        ),
        (
            "window_partition_and_order_flow",
            "SELECT max(o.amount) OVER (ORDER BY o.order_ts) AS m FROM hive.raw.orders o",
            &[("hive.raw.orders", &["amount", "order_ts"])],
        ),
        (
            "filter_only_subquery_table_listed_empty",
            "SELECT o.amount FROM hive.raw.orders o
			 WHERE o.user_id IN (SELECT u.user_id FROM hive.raw.users u)",
            &[("hive.raw.orders", &["amount"]), ("hive.raw.users", &[])],
        ),
        (
            "self_join_single_physical_table",
            "SELECT a.user_name FROM hive.raw.users a JOIN hive.raw.users b ON a.user_id = b.user_id",
            &[("hive.raw.users", &["user_name"])],
        ),
        (
            "cte_chain_resolves_to_roots",
            "WITH a AS (SELECT o.user_id, o.amount FROM hive.raw.orders o WHERE o.status = 'X'),
			      b AS (SELECT a.user_id FROM a JOIN hive.raw.users u ON a.user_id = u.user_id)
			 SELECT b.user_id FROM b",
            &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &[])],
        ),
        (
            "nested_subqueries_resolve",
            "SELECT y.uid FROM (SELECT x.uid FROM (SELECT o.user_id AS uid FROM hive.raw.orders o) x) y",
            &[("hive.raw.orders", &["user_id"])],
        ),
        (
            "union_inside_cte",
            "WITH c AS (SELECT user_id FROM hive.raw.users UNION SELECT user_id FROM hive.raw.orders)
			 SELECT user_id FROM c",
            &[("hive.raw.orders", &["user_id"]), ("hive.raw.users", &["user_id"])],
        ),
        (
            "three_way_union_flat",
            "SELECT user_id FROM hive.raw.users
			 UNION SELECT user_id FROM hive.raw.orders
			 UNION SELECT order_id FROM hive.raw.payments",
            &[
                ("hive.raw.orders", &["user_id"]),
                ("hive.raw.payments", &["order_id"]),
                ("hive.raw.users", &["user_id"]),
            ],
        ),
        (
            "except_right_branch_is_filter_only",
            "SELECT user_id FROM hive.raw.users EXCEPT SELECT user_id FROM hive.raw.orders",
            &[("hive.raw.orders", &[]), ("hive.raw.users", &["user_id"])],
        ),
        (
            "qualified_star_expands_own_table_only",
            "SELECT u.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id",
            &[
                ("hive.raw.orders", &[]),
                ("hive.raw.users", &["email", "user_id", "user_name"]),
            ],
        ),
        (
            "ambiguous_bare_column_metadata_resolves_owner",
            "SELECT email FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id",
            &[("hive.raw.orders", &[]), ("hive.raw.users", &["email"])],
        ),
    ];
    for (name, sql, expected) in cases {
        assert_lineage(name, sql, expected);
    }
}

#[test]
fn bughunt_star_without_metadata() {
    let sql = "SELECT * FROM hive.raw.orders";
    let without = lineage(sql, &options("trino"));
    let unrelated = lineage(
        sql,
        &options("trino").schema([("hive.raw.users", vec!["user_id", "email"])]),
    );
    assert_eq!(without, map(&[("hive.raw.orders", &[])]));
    assert_eq!(unrelated, without, "an unrelated schema entry changed the result");
}

#[test]
fn bughunt_wrapping_statements_match_bare_select() {
    let body = "SELECT u.user_name, o.amount
	              FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id
	              WHERE o.status = 'PAID'";
    let base = lineage(body, &trino());
    assert_eq!(
        base,
        map(&[("hive.raw.orders", &["amount"]), ("hive.raw.users", &["user_name"])])
    );
    for (name, sql) in [
        ("create_view", format!("CREATE VIEW hive.x.v AS {body}")),
        ("ctas", format!("CREATE TABLE hive.x.t AS {body}")),
        ("insert_select", format!("INSERT INTO hive.x.t {body}")),
    ] {
        assert_eq!(lineage(&sql, &trino()), base, "{name}");
    }
}

#[test]
fn bughunt_determinism() {
    let sql = "SELECT email FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id";
    let first = lineage(sql, &options("trino"));
    for _ in 0..4 {
        assert_eq!(lineage(sql, &options("trino")), first);
    }
}

#[test]
fn bughunt_phantom_output_names_credited_as_source_columns() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])])> = vec![
        (
            "count_star_alias",
            "SELECT count(*) AS c FROM hive.raw.orders",
            &[("hive.raw.orders", &[])],
        ),
        (
            "literal_projection",
            "SELECT 1 AS one, 'a' AS lit FROM hive.raw.orders",
            &[("hive.raw.orders", &[])],
        ),
        (
            "unqualified_sum_synthetic_name",
            "SELECT sum(amount) FROM hive.raw.orders GROUP BY status",
            &[("hive.raw.orders", &["amount"])],
        ),
        (
            "correlated_scalar_subquery_alias",
            "SELECT o.order_id,
			        (SELECT max(p.paid_amount) FROM hive.raw.payments p WHERE p.order_id = o.order_id) AS mp
			 FROM hive.raw.orders o",
            &[
                ("hive.raw.orders", &["order_id"]),
                ("hive.raw.payments", &["paid_amount"]),
            ],
        ),
        (
            "literal_union_branch",
            "SELECT 1 AS x UNION SELECT o.amount FROM hive.raw.orders o",
            &[("hive.raw.orders", &["amount"])],
        ),
    ];
    for (name, sql, expected) in cases {
        assert_lineage(name, sql, expected);
    }
}

#[test]
fn bughunt_unrelated_metadata_suffix_leak() {
    let sql = "SELECT amount FROM raw.orders o JOIN raw.users u ON o.id = u.id";
    let control = lineage(sql, &options("trino").schema([("hive.zzz.unrelated", vec!["amount"])]));
    let leaked = lineage(
        sql,
        &options("trino").schema([("hive.braw.orders", vec!["amount", "id"])]),
    );
    assert_eq!(leaked, control, "metadata for hive.braw.orders influenced raw.orders");
}

#[test]
fn bughunt_parenthesized_set_operation_branch_fails() {
    let want = map(&[
        ("hive.raw.orders", &["user_id"]),
        ("hive.raw.payments", &["order_id"]),
        ("hive.raw.users", &["user_id"]),
    ]);
    let flat = "SELECT user_id FROM hive.raw.users
		 UNION SELECT user_id FROM hive.raw.orders
		 UNION SELECT order_id FROM hive.raw.payments";
    assert_eq!(lineage(flat, &trino()), want);
    for (name, sql) in [
        (
            "right_parenthesized",
            "SELECT user_id FROM hive.raw.users UNION (SELECT user_id FROM hive.raw.orders UNION SELECT order_id FROM hive.raw.payments)",
        ),
        (
            "left_parenthesized",
            "(SELECT user_id FROM hive.raw.users UNION SELECT user_id FROM hive.raw.orders) UNION SELECT order_id FROM hive.raw.payments",
        ),
    ] {
        assert_eq!(lineage(sql, &trino()), want, "{name}");
    }
}

// --- lineage_innerquery_test.go --------------------------------------------

#[test]
fn inner_query_unwrap_is_required_by_engine() {
    // The Go test proved the engine rejects these wrappers unless the inner
    // query is unwrapped; here the observable contract is the lineage itself.
    for sql in [
        "CREATE VIEW hive.x.v AS SELECT o.amount FROM hive.raw.orders o",
        "CREATE TABLE hive.x.t AS SELECT o.amount FROM hive.raw.orders o",
        "INSERT INTO hive.x.t SELECT o.amount FROM hive.raw.orders o",
    ] {
        assert_eq!(
            lineage(sql, &trino()),
            map(&[("hive.raw.orders", &["amount"])]),
            "{sql}"
        );
    }
}

#[test]
fn inner_query_non_query_statements_return_nil() {
    for sql in ["CREATE TABLE hive.x.t (id BIGINT, name VARCHAR)", "DROP TABLE hive.x.t"] {
        assert!(lineage(sql, &trino()).is_empty(), "{sql}");
    }
}

#[test]
fn inner_query_unwrap_coverage() {
    let want = map(&[("hive.raw.orders", &["amount"])]);
    for (name, dialect, sql) in [
        (
            "create_view",
            "trino",
            "CREATE VIEW hive.x.v AS SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "create_view_with_column_list",
            "trino",
            "CREATE VIEW hive.x.v (a) AS SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "create_materialized_view",
            "postgres",
            "CREATE MATERIALIZED VIEW mv AS SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "create_table_as_select",
            "trino",
            "CREATE TABLE hive.x.t AS SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "create_table_as_with_cte",
            "trino",
            "CREATE TABLE hive.x.t AS WITH c AS (SELECT o.amount FROM hive.raw.orders o) SELECT amount FROM c",
        ),
        (
            "insert_select",
            "trino",
            "INSERT INTO hive.x.t SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "insert_select_with_cte",
            "trino",
            "INSERT INTO hive.x.t WITH c AS (SELECT o.amount FROM hive.raw.orders o) SELECT amount FROM c",
        ),
        (
            "insert_select_returning",
            "postgres",
            "INSERT INTO t SELECT o.amount FROM hive.raw.orders o RETURNING *",
        ),
        (
            "insert_overwrite",
            "spark",
            "INSERT OVERWRITE TABLE t SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "cache_table_as_select",
            "spark",
            "CACHE TABLE c AS SELECT o.amount FROM hive.raw.orders o",
        ),
        (
            "explain_select",
            "trino",
            "EXPLAIN SELECT o.amount FROM hive.raw.orders o",
        ),
    ] {
        let opts = options(dialect).schema(metadata());
        assert_eq!(lineage(sql, &opts), want, "{name}");
    }
}

#[test]
fn inner_query_unsupported_shapes() {
    let cases: Vec<(&str, &str, &str, &[(&str, &[&str])])> = vec![
        ("insert_values", "trino", "INSERT INTO hive.x.t VALUES (1, 'a')", &[]),
        (
            "create_table_columns",
            "trino",
            "CREATE TABLE hive.x.t (id BIGINT, name VARCHAR)",
            &[],
        ),
        ("create_table_like", "trino", "CREATE TABLE hive.x.t (LIKE hive.raw.orders)", &[]),
        (
            "delete_where_subquery",
            "trino",
            "DELETE FROM hive.x.t WHERE id IN (SELECT o.user_id FROM hive.raw.orders o)",
            &[],
        ),
        (
            "update_set_subquery",
            "trino",
            "UPDATE hive.x.t SET amount = (SELECT max(o.amount) FROM hive.raw.orders o) WHERE id = 1",
            &[("hive.raw.orders", &["amount"]), ("hive.x.t", &[])],
        ),
        (
            "merge_using_select",
            "trino",
            "MERGE INTO hive.x.t USING (SELECT o.user_id, o.amount FROM hive.raw.orders o) s ON t.id = s.user_id WHEN MATCHED THEN UPDATE SET amount = s.amount",
            &[("hive.raw.orders", &["amount"]), ("hive.x.t", &[])],
        ),
        (
            "update_from_select",
            "postgres",
            "UPDATE t SET x = s.v FROM (SELECT o.user_id, o.amount AS v FROM hive.raw.orders o) s WHERE t.id = s.user_id",
            &[("hive.raw.orders", &["amount"]), ("t", &[])],
        ),
    ];
    for (name, dialect, sql, expected) in cases {
        let opts = options(dialect).schema(metadata());
        assert_eq!(lineage(sql, &opts), map(expected), "{name}");
    }
}
