//! Ported from sql-guard `referenced_columns_test.go` and the column-use part
//! of `polyglot_contract_test.go` (ReferencedColumns / ReferencedColumnUsages
//! -> referenced_columns / column_usages).

use std::collections::BTreeMap;

use sqlscope::{column_origins, column_usages, referenced_columns, Clause, ColumnUsage, ErrorKind, Options};

use crate::common::*;

use Clause::*;

fn ref_meta() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("hive.raw.users", vec!["id", "name", "email"]),
        ("hive.raw.orders", vec!["oid", "uid", "amt", "status"]),
        ("c.s.other", vec!["o1", "o2", "o3"]),
    ]
}

fn hunt_meta() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("hive.raw.users", vec!["id", "name", "email"]),
        ("hive.raw.orders", vec!["oid", "uid", "amt", "status"]),
    ]
}

fn trino_meta() -> Vec<(&'static str, Vec<&'static str>)> {
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
    options("trino")
}

fn refs(sql: &str, opts: &Options) -> BTreeMap<String, Vec<String>> {
    referenced_columns(sql, opts).unwrap_or_else(|e| panic!("referenced_columns({sql:?}): {e}"))
}

fn assert_refs(name: &str, sql: &str, expected: &[(&str, &[&str])], opts: Options) {
    assert_eq!(refs(sql, &opts), map(expected), "{name}\nsql: {sql}");
}

fn assert_usages(name: &str, sql: &str, expected: &[(&str, &str, Clause)], opts: Options) {
    let got = column_usages(sql, &opts).unwrap_or_else(|e| panic!("column_usages({sql:?}): {e}"));
    let mut want: Vec<ColumnUsage> = expected
        .iter()
        .map(|(table, column, clause)| ColumnUsage {
            table: table.to_string(),
            column: column.to_string(),
            clause: *clause,
        })
        .collect();
    want.sort();
    assert_eq!(got, want, "{name}\nsql: {sql}");
}

#[test]
fn referenced_columns_includes_filter_columns() {
    assert_refs(
        "where_only_column_included",
        "SELECT a FROM t WHERE b > 1",
        &[("t", &["a", "b"])],
        trino(),
    );
}

#[test]
fn referenced_columns_rejects_multiple_statements() {
    let sql = "SELECT a FROM t; SELECT secret FROM restricted";
    assert_eq!(
        referenced_columns(sql, &trino()).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(column_usages(sql, &trino()).unwrap_err().kind(), ErrorKind::Unsupported);
}

#[test]
fn referenced_columns_is_superset_of_lineage() {
    let sql = "SELECT order_id FROM hive.raw.orders WHERE status = 'X'";
    assert_eq!(
        column_origins(sql, &trino()).unwrap(),
        map(&[("hive.raw.orders", &["order_id"])])
    );
    assert_refs(
        "superset_over_lineage",
        sql,
        &[("hive.raw.orders", &["order_id", "status"])],
        trino(),
    );
}

#[test]
fn referenced_columns_all_filter_positions() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])], Vec<(&str, &str, Clause)>)> = vec![
        (
            "where",
            "SELECT a FROM t WHERE b > 1",
            &[("t", &["a", "b"])],
            vec![("t", "a", Select), ("t", "b", Where)],
        ),
        (
            "group_by",
            "SELECT a, count(*) FROM t GROUP BY a, g",
            &[("t", &["a", "g"])],
            vec![("t", "a", Select), ("t", "a", GroupBy), ("t", "g", GroupBy)],
        ),
        (
            "group_by_projection_alias",
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
            "group_by_ordinal",
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
            "having",
            "SELECT a FROM t GROUP BY a HAVING sum(h) > 1",
            &[("t", &["a", "h"])],
            vec![("t", "a", Select), ("t", "a", GroupBy), ("t", "h", Having)],
        ),
        (
            "order_by",
            "SELECT a FROM t ORDER BY o",
            &[("t", &["a", "o"])],
            vec![("t", "a", Select), ("t", "o", OrderBy)],
        ),
        (
            "order_by_projection_alias",
            "SELECT a AS x FROM t ORDER BY x",
            &[("t", &["a"])],
            vec![("t", "a", Select), ("t", "a", OrderBy)],
        ),
        (
            "order_by_numeric_expression_is_not_ordinal",
            "SELECT a, b FROM t ORDER BY b + 1",
            &[("t", &["a", "b"])],
            vec![("t", "a", Select), ("t", "b", Select), ("t", "b", OrderBy)],
        ),
        (
            "window_partition_and_order",
            "SELECT a, row_number() OVER (PARTITION BY b ORDER BY c) rn FROM t",
            &[("t", &["a", "b", "c"])],
            vec![("t", "a", Select), ("t", "b", Select), ("t", "c", Select)],
        ),
        (
            "named_window_clause",
            "SELECT sum(a) OVER w FROM t WINDOW w AS (PARTITION BY p ORDER BY o)",
            &[("t", &["a", "o", "p"])],
            vec![("t", "a", Select), ("t", "p", Window), ("t", "o", Window)],
        ),
    ];
    for (name, sql, expected, usages) in cases {
        assert_refs(name, sql, expected, trino());
        assert_usages(name, sql, &usages, trino());
    }
}

#[test]
fn referenced_columns_qualify_clause() {
    let snowflake = || options("snowflake");
    let sql = "SELECT a FROM t QUALIFY row_number() OVER (PARTITION BY p ORDER BY q) = 1";
    assert_refs("qualify", sql, &[("t", &["a", "p", "q"])], snowflake());
    assert_usages(
        "qualify",
        sql,
        &[("t", "a", Select), ("t", "p", Qualify), ("t", "q", Qualify)],
        snowflake(),
    );
    let sql = "SELECT a, row_number() OVER (PARTITION BY p ORDER BY q) AS rn FROM t QUALIFY rn = 1";
    assert_refs("qualify_alias", sql, &[("t", &["a", "p", "q"])], snowflake());
    assert_usages(
        "qualify_alias",
        sql,
        &[
            ("t", "a", Select),
            ("t", "p", Select),
            ("t", "p", Qualify),
            ("t", "q", Select),
            ("t", "q", Qualify),
        ],
        snowflake(),
    );
}

#[test]
fn referenced_column_usages_dialect_clauses() {
    for (name, dialect, sql, column, clause) in [
        ("sort_by", "spark", "SELECT a FROM t SORT BY s", "s", SortBy),
        (
            "distribute_by",
            "spark",
            "SELECT a FROM t DISTRIBUTE BY d",
            "d",
            DistributeBy,
        ),
        ("cluster_by", "spark", "SELECT a FROM t CLUSTER BY c", "c", ClusterBy),
        (
            "connect_by",
            "oracle",
            "SELECT a FROM t CONNECT BY PRIOR id = parent_id",
            "id",
            ConnectBy,
        ),
    ] {
        let mut expected = vec![("t", "a", Select), ("t", column, clause)];
        if name == "connect_by" {
            expected.push(("t", "parent_id", ConnectBy));
        }
        assert_usages(name, sql, &expected, options(dialect));
    }
}

#[test]
fn referenced_columns_join_qualified() {
    let sql = "SELECT u.name
         FROM hive.raw.users u
         JOIN hive.raw.orders o ON u.id = o.uid
         WHERE o.status = 'X'
         GROUP BY u.name";
    assert_refs(
        "join_qualified",
        sql,
        &[
            ("hive.raw.users", &["id", "name"]),
            ("hive.raw.orders", &["status", "uid"]),
        ],
        trino().schema(ref_meta()),
    );
    assert_usages(
        "join_qualified",
        sql,
        &[
            ("hive.raw.users", "name", Select),
            ("hive.raw.users", "name", GroupBy),
            ("hive.raw.users", "id", JoinOn),
            ("hive.raw.orders", "uid", JoinOn),
            ("hive.raw.orders", "status", Where),
        ],
        trino().schema(ref_meta()),
    );
}

#[test]
fn referenced_columns_joins_and_qualification() {
    assert_refs(
        "join_unqualified_metadata",
        "SELECT name
         FROM hive.raw.users
         JOIN hive.raw.orders ON id = uid
         WHERE status = 'X'",
        &[
            ("hive.raw.users", &["id", "name"]),
            ("hive.raw.orders", &["status", "uid"]),
        ],
        trino().schema(ref_meta()),
    );
    assert_refs(
        "join_unqualified_no_metadata",
        "SELECT name FROM a JOIN b ON a.k = b.k WHERE status = 'X'",
        &[("a", &["k", "name", "status"]), ("b", &["k", "name", "status"])],
        trino(),
    );
    assert_refs(
        "fully_qualified_same_bare_table_name",
        "SELECT hive.raw.users.id, hive.stage.users.email
         FROM hive.raw.users
         JOIN hive.stage.users ON hive.raw.users.id = hive.stage.users.id",
        &[("hive.raw.users", &["id"]), ("hive.stage.users", &["email", "id"])],
        trino().schema([
            ("hive.raw.users", vec!["id", "name"]),
            ("hive.stage.users", vec!["id", "email"]),
        ]),
    );
    assert_refs(
        "single_table_unqualified",
        "SELECT a, b FROM t WHERE c > 1 GROUP BY a, b",
        &[("t", &["a", "b", "c"])],
        trino(),
    );
    assert_refs(
        "three_part",
        "SELECT hive.raw.users.id FROM hive.raw.users WHERE hive.raw.users.email IS NOT NULL",
        &[("hive.raw.users", &["email", "id"])],
        trino().schema(ref_meta()),
    );
    assert_refs(
        "alias_qualified",
        "SELECT o.amt FROM hive.raw.orders AS o WHERE o.status = 'X'",
        &[("hive.raw.orders", &["amt", "status"])],
        trino().schema(ref_meta()),
    );
}

#[test]
fn referenced_columns_cte() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])], Option<Vec<(&str, &str, Clause)>>)> = vec![
        (
            "single_cte_with_filter",
            "WITH c AS (SELECT a, b FROM t) SELECT c.a FROM c WHERE c.b > 1",
            &[("t", &["a", "b"])],
            Some(vec![("t", "a", Select), ("t", "b", Select), ("t", "b", Where)]),
        ),
        (
            "cte_internal_filter",
            "WITH c AS (SELECT a FROM t WHERE z > 1) SELECT a FROM c",
            &[("t", &["a", "z"])],
            None,
        ),
        (
            "cte_column_aliases",
            "WITH c(x, y) AS (SELECT a, b FROM t) SELECT x FROM c WHERE y > 1",
            &[("t", &["a", "b"])],
            None,
        ),
        (
            "multi_cte",
            "WITH x AS (SELECT a FROM t), y AS (SELECT b FROM r) SELECT x.a, y.b FROM x, y",
            &[("t", &["a"]), ("r", &["b"])],
            None,
        ),
        (
            "cte_referenced_twice",
            "WITH c AS (SELECT a, b FROM t) SELECT c1.a FROM c c1 JOIN c c2 ON c1.a = c2.b",
            &[("t", &["a", "b"])],
            None,
        ),
        (
            "nested_cte",
            "WITH a AS (SELECT x, y FROM t WHERE w > 0), b AS (SELECT x FROM a WHERE y > 0) SELECT x FROM b",
            &[("t", &["w", "x", "y"])],
            None,
        ),
    ];
    for (name, sql, expected, usages) in cases {
        assert_refs(name, sql, expected, trino());
        if let Some(usages) = usages {
            assert_usages(name, sql, &usages, trino());
        }
    }
}

#[test]
fn referenced_columns_subqueries() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])], Option<Vec<(&str, &str, Clause)>>)> = vec![
        (
            "derived_table_in_from",
            "SELECT s.x FROM (SELECT a AS x, b FROM t WHERE c > 1) s WHERE s.x > 0",
            &[("t", &["a", "b", "c"])],
            None,
        ),
        (
            "scalar_subquery_in_select",
            "SELECT a, (SELECT max(x) FROM r) m FROM t",
            &[("t", &["a"]), ("r", &["x"])],
            None,
        ),
        (
            "scalar_subquery_projection_alias_in_order_by",
            "SELECT (SELECT max(x) FROM r) AS m FROM t ORDER BY m",
            &[("t", &[]), ("r", &["x"])],
            Some(vec![("r", "x", Select), ("r", "x", OrderBy)]),
        ),
        (
            "in_subquery",
            "SELECT a FROM t WHERE b IN (SELECT k FROM r WHERE v > 1)",
            &[("t", &["a", "b"]), ("r", &["k", "v"])],
            None,
        ),
        (
            "not_in_subquery",
            "SELECT a FROM t WHERE b NOT IN (SELECT k FROM r)",
            &[("t", &["a", "b"]), ("r", &["k"])],
            None,
        ),
        (
            "exists_correlated",
            "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM r WHERE r.k = t.a)",
            &[("t", &["a"]), ("r", &["k"])],
            None,
        ),
        (
            "scalar_correlated",
            "SELECT a FROM t WHERE b > (SELECT max(x) FROM r WHERE r.k = t.a)",
            &[("t", &["a", "b"]), ("r", &["k", "x"])],
            None,
        ),
        (
            "subquery_in_join",
            "SELECT s.x, o.amt FROM (SELECT a AS x FROM t WHERE c > 1) s JOIN o ON s.x = o.k",
            &[("t", &["a", "c"]), ("o", &["amt", "k"])],
            None,
        ),
    ];
    for (name, sql, expected, usages) in cases {
        assert_refs(name, sql, expected, trino());
        if let Some(usages) = usages {
            assert_usages(name, sql, &usages, trino());
        }
    }
}

#[test]
fn referenced_columns_set_operations() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])])> = vec![
        (
            "union_all",
            "SELECT a FROM t WHERE b > 1 UNION ALL SELECT c FROM r WHERE d < 2",
            &[("t", &["a", "b"]), ("r", &["c", "d"])],
        ),
        (
            "intersect",
            "SELECT a FROM t WHERE x > 1 INTERSECT SELECT b FROM r WHERE y < 2",
            &[("t", &["a", "x"]), ("r", &["b", "y"])],
        ),
        (
            "except",
            "SELECT a FROM t EXCEPT SELECT b FROM r",
            &[("t", &["a"]), ("r", &["b"])],
        ),
        (
            "with_then_union",
            "WITH c AS (SELECT a, b FROM t) SELECT a FROM c WHERE b > 1 UNION ALL SELECT x FROM r WHERE y < 2",
            &[("t", &["a", "b"]), ("r", &["x", "y"])],
        ),
    ];
    for (name, sql, expected) in cases {
        assert_refs(name, sql, expected, trino());
    }
}

#[test]
fn referenced_columns_star() {
    assert_refs(
        "bare_star_no_metadata",
        "SELECT * FROM t WHERE id > 0",
        &[("t", &["*", "id"])],
        trino(),
    );
    let sql = "SELECT * FROM hive.raw.users WHERE id > 0";
    assert_refs(
        "bare_star_with_metadata",
        sql,
        &[("hive.raw.users", &["email", "id", "name"])],
        trino().schema(ref_meta()),
    );
    assert_usages(
        "bare_star_with_metadata",
        sql,
        &[
            ("hive.raw.users", "id", Select),
            ("hive.raw.users", "name", Select),
            ("hive.raw.users", "email", Select),
            ("hive.raw.users", "id", Where),
        ],
        trino().schema(ref_meta()),
    );
    assert_refs(
        "qualified_star_with_metadata",
        "SELECT u.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
        &[
            ("hive.raw.users", &["email", "id", "name"]),
            ("hive.raw.orders", &["uid"]),
        ],
        trino().schema(ref_meta()),
    );
    assert_refs(
        "bare_star_join_with_metadata",
        "SELECT * FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
        &[
            ("hive.raw.users", &["email", "id", "name"]),
            ("hive.raw.orders", &["amt", "oid", "status", "uid"]),
        ],
        trino().schema(ref_meta()),
    );
}

#[test]
fn referenced_columns_ddl_wrappers() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])])> = vec![
        (
            "create_view",
            "CREATE VIEW v AS SELECT a FROM t WHERE b > 1",
            &[("t", &["a", "b"])],
        ),
        (
            "create_table_as_select",
            "CREATE TABLE d AS SELECT a FROM t WHERE b > 1",
            &[("t", &["a", "b"])],
        ),
        (
            "insert_select",
            "INSERT INTO d SELECT a FROM t WHERE b > 1",
            &[("t", &["a", "b"])],
        ),
        (
            "create_view_with_cte_and_join",
            "CREATE VIEW v AS WITH c AS (SELECT a, b FROM t WHERE z > 0) SELECT c.a FROM c JOIN r ON c.b = r.k",
            &[("t", &["a", "b", "z"]), ("r", &["k"])],
        ),
    ];
    for (name, sql, expected) in cases {
        assert_refs(name, sql, expected, trino());
    }
}

#[test]
fn referenced_columns_table_with_no_columns_still_appears() {
    assert_refs("select_constant", "SELECT 1 FROM t", &[("t", &[])], trino());
    assert_usages("select_constant", "SELECT 1 FROM t", &[], trino());
}

#[test]
fn referenced_columns_non_query_statements_are_empty() {
    for sql in ["DROP TABLE t", "CREATE TABLE t (a INT, b INT)"] {
        assert!(refs(sql, &trino()).is_empty(), "{sql}");
        assert!(column_usages(sql, &trino()).unwrap().is_empty(), "{sql}");
    }
}

#[test]
fn referenced_columns_expressions() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])])> = vec![
        (
            "case_expression",
            "SELECT CASE WHEN a > 1 THEN b ELSE c END FROM t",
            &[("t", &["a", "b", "c"])],
        ),
        ("function_args", "SELECT coalesce(a, b) FROM t", &[("t", &["a", "b"])]),
        ("arithmetic", "SELECT a + b * c FROM t", &[("t", &["a", "b", "c"])]),
        (
            "between",
            "SELECT a FROM t WHERE x BETWEEN y AND z",
            &[("t", &["a", "x", "y", "z"])],
        ),
        (
            "in_list",
            "SELECT a FROM t WHERE x IN (y, z)",
            &[("t", &["a", "x", "y", "z"])],
        ),
        (
            "group_by_expression",
            "SELECT a + b FROM t GROUP BY a + b",
            &[("t", &["a", "b"])],
        ),
        (
            "window_frame",
            "SELECT sum(amt) OVER (PARTITION BY p ORDER BY o ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
            &[("t", &["amt", "o", "p"])],
        ),
    ];
    for (name, sql, expected) in cases {
        assert_refs(name, sql, expected, trino());
    }
}

#[test]
fn referenced_columns_from_constructs() {
    let sql = "SELECT a FROM t JOIN r USING (k)";
    assert_refs("using", sql, &[("t", &["a", "k"]), ("r", &["a", "k"])], trino());
    assert_usages(
        "using",
        sql,
        &[
            ("t", "a", Select),
            ("r", "a", Select),
            ("t", "k", JoinUsing),
            ("r", "k", JoinUsing),
        ],
        trino(),
    );
    assert_refs(
        "self_join",
        "SELECT a.x, b.y FROM t a JOIN t b ON a.id = b.id",
        &[("t", &["id", "x", "y"])],
        trino(),
    );
    let sql = "SELECT i FROM t, UNNEST(t.arr) AS x(i)";
    assert_refs("unnest", sql, &[("t", &["arr", "i"])], trino());
    assert_usages("unnest", sql, &[("t", "arr", From), ("t", "i", Select)], trino());
    assert_refs(
        "pivot",
        "SELECT * FROM (SELECT region, amt FROM s) PIVOT (sum(amt) FOR region IN ('A'))",
        &[("s", &["amt", "region"])],
        trino(),
    );
    let sql = "SELECT e FROM t LATERAL VIEW explode(t.arr) tbl AS e";
    assert_refs("lateral_view", sql, &[("t", &["arr", "e"])], options("spark"));
    assert_usages(
        "lateral_view",
        sql,
        &[("t", "arr", LateralView), ("t", "e", Select)],
        options("spark"),
    );
}

#[test]
fn referenced_columns_never_drops_unresolvable() {
    assert_refs("unknown_qualifier", "SELECT x.a FROM t", &[("t", &["a"])], trino());
    assert_refs(
        "unknown_column_no_metadata",
        "SELECT name FROM a JOIN b ON a.k = b.k WHERE foo > 1",
        &[("a", &["foo", "k", "name"]), ("b", &["foo", "k", "name"])],
        trino(),
    );
    assert_refs(
        "incomplete_metadata",
        "SELECT u.id FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid WHERE foo > 1",
        &[("hive.raw.users", &["foo", "id"]), ("hive.raw.orders", &["foo", "uid"])],
        trino().schema(ref_meta()),
    );
    assert_refs(
        "derived_star_passthrough",
        "SELECT s.x FROM (SELECT * FROM t) s",
        &[("t", &["*", "x"])],
        trino(),
    );
}

#[test]
fn referenced_columns_dml() {
    let cases: Vec<(&str, &str, &str, &[(&str, &[&str])], Vec<(&str, &str, Clause)>)> = vec![
        (
            "delete_where",
            "trino",
            "DELETE FROM t WHERE a > 1",
            &[("t", &["a"])],
            vec![("t", "a", Where)],
        ),
        (
            "delete_using",
            "postgres",
            "DELETE FROM t USING r WHERE t.id = r.id AND r.k > 1",
            &[("t", &["id"]), ("r", &["id", "k"])],
            vec![("t", "id", Where), ("r", "id", Where), ("r", "k", Where)],
        ),
        (
            "update_set_and_where",
            "trino",
            "UPDATE t SET x = y + 1 WHERE a > 1",
            &[("t", &["a", "x", "y"])],
            vec![("t", "x", UpdateSetTarget), ("t", "y", UpdateSetValue), ("t", "a", Where)],
        ),
        (
            "update_from",
            "postgres",
            "UPDATE t SET x = s.v FROM r s WHERE t.id = s.id",
            &[("t", &["id", "x"]), ("r", &["id", "v"])],
            vec![
                ("t", "x", UpdateSetTarget),
                ("r", "v", UpdateSetValue),
                ("t", "id", Where),
                ("r", "id", Where),
            ],
        ),
        (
            "merge",
            "trino",
            "MERGE INTO t USING r ON t.id = r.id WHEN MATCHED THEN UPDATE SET x = r.v WHEN NOT MATCHED THEN INSERT (a) VALUES (r.b)",
            &[("t", &["id"]), ("r", &["b", "id", "v"])],
            vec![("t", "id", MergeOn), ("r", "id", MergeOn), ("r", "v", MergeWhen), ("r", "b", MergeWhen)],
        ),
        (
            "merge_with_subquery_source",
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
    for (name, dialect, sql, expected, usages) in cases {
        assert_refs(name, sql, expected, options(dialect));
        assert_usages(name, sql, &usages, options(dialect));
    }
}

#[test]
fn referenced_columns_misc_edges() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])])> = vec![
        (
            "deeply_nested_subqueries",
            "SELECT z FROM (SELECT y AS z FROM (SELECT x AS y FROM t WHERE w > 0) i WHERE i.y > 0) o WHERE o.z > 0",
            &[("t", &["w", "x"])],
        ),
        (
            "recursive_cte_terminates",
            "WITH RECURSIVE c AS (SELECT a FROM t UNION ALL SELECT c.a FROM c WHERE c.a > 0) SELECT a FROM c",
            &[("t", &["a"])],
        ),
        (
            "quoted_identifiers",
            r#"SELECT "Order" FROM t WHERE "User" > 1"#,
            &[("t", &["Order", "User"])],
        ),
        (
            "insert_target_columns_excluded",
            "INSERT INTO d (x, y) SELECT a, b FROM t WHERE c > 1",
            &[("t", &["a", "b", "c"])],
        ),
    ];
    for (name, sql, expected) in cases {
        assert_refs(name, sql, expected, trino());
    }
}

/// Every column `column_origins` reports must also be referenced.
fn assert_superset(sql: &str, opts: &Options) {
    let lineage = column_origins(sql, opts).unwrap_or_else(|e| panic!("column_origins({sql:?}): {e}"));
    let referenced = refs(sql, opts);
    assert!(!lineage.is_empty(), "{sql}: no lineage tables");
    assert!(!referenced.is_empty(), "{sql}: no referenced tables");
    for (table, columns) in &lineage {
        let referenced_columns = referenced
            .get(table)
            .unwrap_or_else(|| panic!("{sql}: {table} in lineage but not referenced ({referenced:?})"));
        for column in columns {
            assert!(
                referenced_columns.contains(column),
                "{sql}: {table}.{column} in lineage but not referenced {referenced_columns:?}"
            );
        }
    }
}

#[test]
fn referenced_columns_superset_invariant() {
    for sql in [
        "SELECT user_id FROM hive.raw.orders WHERE status = 'X'",
        "SELECT u.user_name FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id WHERE o.status = 'PAID'",
        "CREATE VIEW v AS SELECT o.amount FROM hive.raw.orders o WHERE o.order_date >= DATE '2024-01-01'",
        "WITH p AS (SELECT order_id, user_id FROM hive.raw.orders WHERE status = 'PAID') SELECT order_id FROM p WHERE user_id > 0",
        "SELECT user_id FROM hive.raw.users UNION SELECT user_id FROM hive.raw.orders",
        "CREATE TABLE d AS SELECT o.amount FROM hive.raw.orders o GROUP BY o.amount HAVING count(o.order_id) > 1",
        "INSERT INTO d SELECT u.user_name FROM hive.raw.users u WHERE u.email IS NOT NULL ORDER BY u.user_id",
        "SELECT amount FROM hive.raw.orders o WHERE o.user_id IN (SELECT user_id FROM hive.raw.users WHERE email LIKE '%@x.com')",
        "SELECT * FROM hive.raw.orders WHERE status = 'PAID'",
        "SELECT o.amount, u.user_name FROM hive.raw.orders o JOIN hive.raw.users u ON o.user_id = u.user_id WHERE u.email IS NOT NULL GROUP BY o.amount, u.user_name",
    ] {
        assert_superset(sql, &trino().schema(trino_meta()));
    }
}

#[test]
fn bughunt_superset_property() {
    for sql in [
        "SELECT id FROM hive.raw.users WHERE email IS NOT NULL",
        "SELECT u.name, o.amt FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
        "SELECT name FROM hive.raw.users ORDER BY id",
        "SELECT status, sum(amt) FROM hive.raw.orders GROUP BY status HAVING count(oid) > 1",
        "WITH c AS (SELECT id, name FROM hive.raw.users WHERE email LIKE '%x') SELECT name FROM c WHERE id > 0",
        "SELECT * FROM hive.raw.orders WHERE status = 'PAID'",
        "SELECT id FROM hive.raw.users UNION ALL SELECT uid FROM hive.raw.orders",
        "SELECT s.n FROM (SELECT name AS n, id FROM hive.raw.users) s WHERE s.id > 1",
        "SELECT amt FROM hive.raw.orders o WHERE o.uid IN (SELECT id FROM hive.raw.users WHERE name = 'a')",
        "SELECT id, row_number() OVER (PARTITION BY name ORDER BY email) FROM hive.raw.users",
        "SELECT a.id FROM hive.raw.users a JOIN hive.raw.users b ON a.email = b.name",
    ] {
        let opts = trino().schema(hunt_meta());
        assert_superset(sql, &opts);
        // sql-guard once leaked synthesized positional names (_0, _1, ...).
        for columns in column_origins(sql, &opts).unwrap().values() {
            assert!(
                columns
                    .iter()
                    .all(|c| !(c.starts_with('_') && c[1..].bytes().all(|b| b.is_ascii_digit()))),
                "{sql}: synthesized name in lineage {columns:?}"
            );
        }
    }
}

#[test]
fn bughunt_scoping_edges() {
    let cases: Vec<(&str, &str, &[(&str, &[&str])], Options)> = vec![
        (
            "cte_shadow_self_reference",
            "WITH t AS (SELECT a FROM t) SELECT a FROM t",
            &[("t", &["a"])],
            trino(),
        ),
        (
            "using_both_sides",
            "SELECT u.name FROM hive.raw.users u JOIN hive.raw.orders o USING (id)",
            &[("hive.raw.users", &["id", "name"]), ("hive.raw.orders", &["id"])],
            trino().schema([("hive.raw.users", vec!["id", "name"]), ("hive.raw.orders", vec!["id", "amt"])]),
        ),
        (
            "unqualified_in_two_tables_metadata",
            "SELECT k FROM a JOIN b ON a.x = b.y",
            &[("a", &["k", "x"]), ("b", &["k", "y"])],
            trino().schema([("a", vec!["k", "x"]), ("b", vec!["k", "y"])]),
        ),
        (
            "correlated_unqualified_metadata",
            "SELECT u.id FROM hive.raw.users u WHERE EXISTS (SELECT 1 FROM hive.raw.orders o WHERE o.uid = u.id AND name = 'x')",
            &[("hive.raw.users", &["id", "name"]), ("hive.raw.orders", &["uid"])],
            trino().schema(hunt_meta()),
        ),
        (
            "nested_set_ops",
            "SELECT a FROM t WHERE p > 0 UNION SELECT b FROM r EXCEPT SELECT c FROM s WHERE q < 1",
            &[("t", &["a", "p"]), ("r", &["b"]), ("s", &["c", "q"])],
            trino(),
        ),
        (
            "qualify_window",
            "SELECT a FROM t QUALIFY row_number() OVER (PARTITION BY p ORDER BY q DESC) = 1",
            &[("t", &["a", "p", "q"])],
            options("snowflake"),
        ),
        (
            "cte_twice_different_columns",
            "WITH c AS (SELECT a, b, d FROM t) SELECT c1.a FROM c c1 JOIN c c2 ON c1.b = c2.d",
            &[("t", &["a", "b", "d"])],
            trino(),
        ),
        (
            "cte_shadows_physical_name",
            "WITH orders AS (SELECT uid FROM hive.raw.orders WHERE status = 'X') SELECT uid FROM orders",
            &[("hive.raw.orders", &["status", "uid"])],
            trino(),
        ),
        (
            "self_join_distinct_columns",
            "SELECT l.a, r.b FROM t l JOIN t r ON l.k1 = r.k2 WHERE l.w > 0 ORDER BY r.o",
            &[("t", &["a", "b", "k1", "k2", "o", "w"])],
            trino(),
        ),
        (
            "update_qualified_set_target",
            "UPDATE t SET t.x = t.y + 1 WHERE t.a > 1",
            &[("t", &["a", "x", "y"])],
            options("mysql"),
        ),
        (
            "delete_with_subquery",
            "DELETE FROM t WHERE id IN (SELECT uid FROM r WHERE flag = 1)",
            &[("t", &["id"]), ("r", &["flag", "uid"])],
            trino(),
        ),
        (
            "merge_when_condition",
            "MERGE INTO t USING r ON t.id = r.id WHEN MATCHED AND r.flag = 1 THEN DELETE",
            &[("t", &["id"]), ("r", &["flag", "id"])],
            trino(),
        ),
        (
            "having_aggregate",
            "SELECT g FROM t GROUP BY g HAVING max(h) - min(h2) > 3",
            &[("t", &["g", "h", "h2"])],
            trino(),
        ),
        (
            "star_partial_metadata",
            "SELECT * FROM hive.raw.users u JOIN unknown_tbl x ON u.id = x.k",
            &[("hive.raw.users", &["id", "name"]), ("unknown_tbl", &["*", "k"])],
            trino().schema([("hive.raw.users", vec!["id", "name"])]),
        ),
        (
            "three_part_qualified",
            "SELECT c.s.t1.a FROM c.s.t1 JOIN c.s.t2 ON c.s.t1.k = c.s.t2.k WHERE c.s.t2.f > 0",
            &[("c.s.t1", &["a", "k"]), ("c.s.t2", &["f", "k"])],
            trino(),
        ),
    ];
    for (name, sql, expected, opts) in cases {
        assert_refs(name, sql, expected, opts);
    }
}

#[test]
fn bughunt_cte_forward_sibling_reference() {
    let got = refs(
        "WITH a AS (SELECT x FROM b), b AS (SELECT y FROM t) SELECT x FROM a",
        &trino(),
    );
    assert!(
        got.contains_key("b"),
        "forward sibling must be the physical table b: {got:?}"
    );
}

#[test]
fn bughunt_unknown_qualifier_star_dropped() {
    let got = refs("SELECT x.* FROM t", &trino());
    assert!(got.get("t").is_some_and(|c| !c.is_empty()), "{got:?}");
}

#[test]
fn bughunt_correlated_qualified_star() {
    let got = refs(
        "SELECT a FROM t WHERE EXISTS (SELECT t.* FROM r WHERE r.k = t.a)",
        &trino(),
    );
    assert!(got["t"].iter().any(|c| c == "*"), "{got:?}");
}

#[test]
fn bughunt_set_op_order_by() {
    let sql = "SELECT a FROM t UNION ALL SELECT b FROM r ORDER BY a";
    assert_refs("set_op_order_by", sql, &[("t", &["a"]), ("r", &["b"])], trino());
    assert_usages(
        "set_op_order_by",
        sql,
        &[
            ("t", "a", Select),
            ("t", "a", OrderBy),
            ("r", "b", Select),
            ("r", "b", OrderBy),
        ],
        trino(),
    );
}

#[test]
fn bughunt_order_by_ordinal() {
    let sql = "SELECT a, b FROM t ORDER BY 1, b";
    assert_refs("order_by_ordinal", sql, &[("t", &["a", "b"])], trino());
    assert_usages(
        "order_by_ordinal",
        sql,
        &[
            ("t", "a", Select),
            ("t", "a", OrderBy),
            ("t", "b", Select),
            ("t", "b", OrderBy),
        ],
        trino(),
    );
}

#[test]
fn bughunt_dedup_and_sorted() {
    let sql = "SELECT b, a, b FROM t WHERE a > 1 AND b < 2 ORDER BY a";
    let got = refs(sql, &trino());
    assert_eq!(got, map(&[("t", &["a", "b"])]));
    assert!(got.values().all(|c| c.windows(2).all(|w| w[0] <= w[1])), "{got:?}");
    assert_usages(
        "usage_dedup_and_sort",
        sql,
        &[
            ("t", "b", Select),
            ("t", "a", Select),
            ("t", "b", Where),
            ("t", "a", Where),
            ("t", "a", OrderBy),
        ],
        trino(),
    );
}

#[test]
fn native_column_uses_compatibility_boundary() {
    assert_refs(
        "inner_projection_is_still_a_reference",
        "WITH c AS (SELECT id, amount, unused FROM orders) SELECT id FROM c WHERE amount > 0",
        &[("orders", &["id", "amount", "unused"])],
        trino(),
    );
}
