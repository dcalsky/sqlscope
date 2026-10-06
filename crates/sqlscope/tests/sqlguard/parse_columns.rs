//! Ported from sql-guard `parse_columns_test.go` and the output-column parts
//! of `polyglot_contract_test.go` (ParseColumns -> output_columns). The
//! contract tests called the polyglot client directly; they are ported as
//! the same assertions through the public API.

use sqlscope::{output_columns, ErrorKind, Options};

use crate::common::*;

fn view_meta() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("hive.raw.users", vec!["id", "name", "email"]),
        ("hive.raw.orders", vec!["oid", "uid", "amt"]),
        ("c.s.other", vec!["o1", "o2", "o3"]),
    ]
}

fn pc_meta() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("hive.raw.users", vec!["id", "name", "email"]),
        ("hive.raw.orders", vec!["oid", "uid", "amt"]),
    ]
}

fn users() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![("hive.raw.users", vec!["id", "name", "email"])]
}

fn trino() -> Options {
    options("trino")
}

fn columns(sql: &str, opts: &Options) -> Option<Vec<String>> {
    output_columns(sql, opts).unwrap_or_else(|e| panic!("output_columns({sql:?}): {e}"))
}

fn check(name: &str, sql: &str, expected: &[&str], opts: Options) {
    assert_eq!(
        columns(sql, &opts),
        Some(expected.iter().map(|c| c.to_string()).collect()),
        "{name}"
    );
}

fn unsupported(name: &str, sql: &str, opts: Options) {
    let error = output_columns(sql, &opts).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{name}: {error}");
}

/// The view from sql-guard's `realViewSQL`, line for line.
fn real_view() -> (String, Vec<String>) {
    let mut sql = String::from(
        "\nCREATE VIEW vdm_mda.weiava.target_hcp_cnt AS\nSELECT\n  hco_province_cn\n, hco_tier_cn\n, standard_department_nm\n, is_core_department\n, bu\n, brand\n",
    );
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
    let ranges = |year: u32, ytd: bool| -> Vec<(String, String)> {
        let starts = if ytd {
            ["01", "01", "01", "01"]
        } else {
            ["01", "04", "07", "10"]
        };
        let ends = ["03", "06", "09", "12"];
        (0..4)
            .map(|q| (format!("{year}{}", starts[q]), format!("{year}{}", ends[q])))
            .collect()
    };
    for (prefix, value) in [("hcp_hco", "concat(hcp_etms_cd, hco_etms_cd)"), ("hcp", "hcp_etms_cd")] {
        for year in [2025, 2026] {
            for ytd in [false, true] {
                for (quarter, (from, to)) in ranges(year, ytd).into_iter().enumerate() {
                    let kind = if ytd { "YTD Target" } else { "Target" };
                    let alias = format!("{prefix} {year} Q{} {kind}", quarter + 1);
                    sql.push_str(&format!(
                        ", count(DISTINCT (CASE WHEN (yearmonth BETWEEN '{from}' AND '{to}') THEN {value} END)) \"{alias}\"\n"
                    ));
                    expected.push(alias);
                }
            }
        }
    }
    sql.push_str(
        "FROM\n  iceberg.rda_launch_to_engage.target_hcp\nWHERE (yearmonth >= '202501')\nGROUP BY 1, 2, 3, 4, 5, 6\nORDER BY 1 ASC, 2 ASC, 3 ASC, 4 ASC, 5 ASC, 6 ASC\n",
    );
    (sql, expected)
}

#[test]
fn parse_columns_rejects_multiple_statements() {
    unsupported(
        "multiple_statements",
        "SELECT a FROM t; SELECT secret FROM restricted",
        trino(),
    );
}

#[test]
fn parse_columns_real_view_bare_and_quoted_alias_columns() {
    let (sql, expected) = real_view();
    assert!(
        sql.contains("'202510' AND '202512') THEN concat(hcp_etms_cd, hco_etms_cd) END)) \"hcp_hco 2025 Q4 Target\"")
    );
    assert!(sql.contains("'202601' AND '202609') THEN hcp_etms_cd END)) \"hcp 2026 Q3 YTD Target\""));
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    check("real_view_bare_and_quoted_alias_columns", &sql, &expected, trino());
}

#[test]
fn parse_columns_real_view_with_comment_and_security_definer() {
    let (sql, expected) = real_view();
    let sql = sql.replacen("AS\nSELECT", "SECURITY DEFINER AS\nSELECT", 1);
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    check("real_view_with_comment_and_security_definer", &sql, &expected, trino());
}

#[test]
fn parse_columns_projection_names() {
    // One Go test per case in parse_columns_test.go.
    let cases: Vec<(&str, &str, &[&str])> = vec![
        (
            "unaliased_expressions_become_col_index",
            "CREATE VIEW c.s.v AS SELECT a, count(*), sum(x) FROM t GROUP BY 1",
            &["a", "_col1", "_col2"],
        ),
        (
            "explicit_view_column_list_overrides_select_names",
            "CREATE VIEW c.s.v (x1, x2, x3) AS SELECT a, b, count(*) FROM t GROUP BY 1, 2",
            &["x1", "x2", "x3"],
        ),
        (
            "bare_select_without_create_view",
            "SELECT a AS x, b FROM t",
            &["x", "b"],
        ),
        (
            "union_takes_names_from_leftmost_select",
            "SELECT a, count(*) FROM t GROUP BY 1 UNION ALL SELECT c, d FROM r",
            &["a", "_col1"],
        ),
        (
            "cte_view_uses_final_select_names",
            "
        CREATE VIEW c.s.v AS
        WITH paid AS (SELECT order_id, amount FROM orders WHERE status = 'PAID')
        SELECT order_id AS oid, amount AS amt FROM paid
        ",
            &["oid", "amt"],
        ),
        (
            "star_without_metadata_returns_star",
            "SELECT * FROM hive.raw.users",
            &["*"],
        ),
        (
            "ctas_bare_and_unaliased_columns",
            "CREATE TABLE c.s.t AS SELECT a, b, count(*) FROM r GROUP BY 1, 2",
            &["a", "b", "_col2"],
        ),
        (
            "ctas_with_explicit_column_list",
            "CREATE TABLE c.s.t (x1, x2) AS SELECT a, b FROM r",
            &["x1", "x2"],
        ),
        (
            "ctas_with_quoted_alias",
            r#"CREATE TABLE c.s.t AS SELECT a, count(*) "Total Cnt" FROM r GROUP BY 1"#,
            &["a", "Total Cnt"],
        ),
        (
            "create_or_replace_table_ctas",
            "CREATE OR REPLACE TABLE c.s.t AS SELECT x AS y, z FROM r",
            &["y", "z"],
        ),
        (
            "create_table_ddl_column_defs",
            "CREATE TABLE c.s.t (id BIGINT, name VARCHAR, created_at TIMESTAMP)",
            &["id", "name", "created_at"],
        ),
        (
            "create_table_ddl_with_table_properties",
            "CREATE TABLE c.s.t (a INTEGER, b VARCHAR) WITH (format = 'PARQUET', partitioning = ARRAY['a'])",
            &["a", "b"],
        ),
        (
            "create_table_ddl_with_column_comments_and_constraints",
            "CREATE TABLE c.s.t (id BIGINT NOT NULL COMMENT 'pk', name VARCHAR COMMENT 'n')",
            &["id", "name"],
        ),
        (
            "create_table_if_not_exists_ddl",
            "CREATE TABLE IF NOT EXISTS c.s.t (a INT, b INT)",
            &["a", "b"],
        ),
        (
            "create_table_as_with_cte",
            "CREATE TABLE c.s.t AS WITH c AS (SELECT a, b FROM r) SELECT a, count(*) FROM c GROUP BY 1",
            &["a", "_col1"],
        ),
        (
            "create_materialized_view",
            "CREATE MATERIALIZED VIEW c.s.mv AS SELECT a, sum(b) total FROM r GROUP BY 1",
            &["a", "total"],
        ),
        (
            "create_or_replace_view",
            "CREATE OR REPLACE VIEW c.s.v AS SELECT a, b FROM r",
            &["a", "b"],
        ),
        (
            "with_single_cte",
            "WITH cte AS (SELECT a, b FROM r) SELECT a AS x, b FROM cte",
            &["x", "b"],
        ),
        (
            "with_multiple_ctes",
            "WITH c1 AS (SELECT a FROM r), c2 AS (SELECT b FROM s) SELECT c1.a, c2.b FROM c1, c2",
            &["a", "b"],
        ),
        (
            "with_cte_column_aliases",
            "WITH c(x, y) AS (SELECT a, b FROM r) SELECT x, y FROM c",
            &["x", "y"],
        ),
        (
            "with_then_union",
            "WITH c AS (SELECT a, b FROM r) SELECT a, count(*) FROM c GROUP BY 1 UNION ALL SELECT x, y FROM s",
            &["a", "_col1"],
        ),
        (
            "create_view_with_cte",
            "CREATE VIEW c.s.v AS WITH c AS (SELECT a, b FROM r) SELECT a AS oid, b AS amt FROM c",
            &["oid", "amt"],
        ),
        (
            "insert_select_uses_select_output_columns",
            "INSERT INTO c.s.t SELECT a, b FROM r",
            &["a", "b"],
        ),
        (
            "insert_with_target_column_list",
            "INSERT INTO c.s.t (col1, col2) SELECT a, b FROM r",
            &["col1", "col2"],
        ),
        (
            "insert_values_with_column_list",
            "INSERT INTO c.s.t (a, b) VALUES (1, 2), (3, 4)",
            &["a", "b"],
        ),
    ];
    for (name, sql, expected) in cases {
        check(name, sql, expected, trino());
    }
}

#[test]
fn parse_columns_wildcards_with_metadata() {
    let cases: Vec<(&str, &str, &[&str], Options)> = vec![
        (
            "star_with_explicit_empty_table_metadata_returns_empty",
            "SELECT * FROM hive.raw.users",
            &[],
            trino().schema([("hive.raw.users", Vec::<&str>::new())]),
        ),
        (
            "qualified_star_with_explicit_empty_table_metadata_returns_empty",
            "SELECT u.* FROM hive.raw.users u",
            &[],
            trino().schema([("hive.raw.users", Vec::<&str>::new())]),
        ),
        (
            "star_with_metadata_is_expanded",
            "SELECT u.name, o.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
            &["name", "oid", "uid", "amt"],
            trino().schema(view_meta()),
        ),
        (
            "create_view_select_star_single_table_with_metadata",
            "CREATE VIEW hive.analytics.v AS SELECT * FROM hive.raw.users",
            &["id", "name", "email"],
            trino().schema(users()),
        ),
        (
            "create_view_select_star_join_with_metadata",
            "CREATE VIEW hive.analytics.v AS SELECT * FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
            &["id", "name", "email", "oid", "uid", "amt"],
            trino().schema(view_meta()),
        ),
        (
            "create_view_qualified_star_with_metadata",
            "CREATE VIEW hive.analytics.v AS SELECT u.*, o.amt FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
            &["id", "name", "email", "amt"],
            trino().schema(view_meta()),
        ),
        (
            "create_view_select_star_partial_metadata_falls_back_to_star",
            "CREATE VIEW hive.analytics.v AS SELECT * FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
            &["*"],
            trino().schema(users()),
        ),
        (
            "create_table_like_with_metadata_expands",
            "CREATE TABLE c.s.t (LIKE c.s.other)",
            &["o1", "o2", "o3"],
            trino().schema(view_meta()),
        ),
        (
            "create_table_like_including_properties_with_metadata",
            "CREATE TABLE c.s.t (LIKE c.s.other INCLUDING PROPERTIES)",
            &["o1", "o2", "o3"],
            trino().schema(view_meta()),
        ),
        (
            "create_table_mixed_columns_and_like_with_metadata",
            "CREATE TABLE c.s.t (a INT, LIKE c.s.other, b VARCHAR)",
            &["a", "o1", "o2", "o3", "b"],
            trino().schema(view_meta()),
        ),
        (
            "create_table_like_matches_by_name_suffix",
            "CREATE TABLE c.s.t (LIKE other)",
            &["o1", "o2", "o3"],
            trino().schema(view_meta()),
        ),
        (
            "with_star_expanded_with_metadata",
            "WITH c AS (SELECT * FROM hive.raw.users) SELECT * FROM c",
            &["id", "name", "email"],
            trino().schema(users()),
        ),
        (
            "with_join_star_with_metadata",
            "
        WITH c AS (
            SELECT * FROM hive.raw.users u
            JOIN hive.raw.orders o ON u.id = o.uid
        ) SELECT * FROM c",
            &["id", "name", "email", "oid", "uid", "amt"],
            trino().schema(view_meta()),
        ),
        (
            "with_qualified_star_in_cte_with_metadata",
            "
        WITH c AS (
            SELECT u.*, o.amt FROM hive.raw.users u
            JOIN hive.raw.orders o ON u.id = o.uid
        ) SELECT * FROM c",
            &["id", "name", "email", "amt"],
            trino().schema(view_meta()),
        ),
        (
            "with_outer_qualified_star_with_metadata",
            "WITH c AS (SELECT * FROM hive.raw.users) SELECT c.* FROM c",
            &["id", "name", "email"],
            trino().schema(users()),
        ),
        (
            "with_join_star_partial_metadata_falls_back_to_star",
            "
        WITH c AS (
            SELECT * FROM hive.raw.users u
            JOIN hive.raw.orders o ON u.id = o.uid
        ) SELECT * FROM c",
            &["*"],
            trino().schema(users()),
        ),
        (
            "create_view_as_with_join_star_with_metadata",
            "
        CREATE VIEW hive.analytics.v AS
        WITH c AS (
            SELECT * FROM hive.raw.users u
            JOIN hive.raw.orders o ON u.id = o.uid
        ) SELECT * FROM c",
            &["id", "name", "email", "oid", "uid", "amt"],
            trino().schema(view_meta()),
        ),
        (
            "insert_select_star_with_metadata",
            "INSERT INTO c.s.t SELECT * FROM hive.raw.users",
            &["id", "name", "email"],
            trino().schema(users()),
        ),
    ];
    for (name, sql, expected, opts) in cases {
        check(name, sql, expected, opts);
    }
}

#[test]
fn parse_columns_unsupported_statements() {
    unsupported(
        "create_table_like_without_metadata_raises",
        "CREATE TABLE c.s.t (LIKE c.s.other)",
        trino(),
    );
    unsupported(
        "insert_values_without_column_list_raises",
        "INSERT INTO c.s.t VALUES (1, 2)",
        trino(),
    );
}

#[test]
fn parse_columns_drop_returns_nil() {
    assert_eq!(columns("DROP TABLE hive.x.t", &trino()), None);
}

#[test]
fn bughunt_projection_order_and_names() {
    let meta = || trino().schema(pc_meta());
    let cases: Vec<(&str, &str, &[&str], Options)> = vec![
        (
            "explicit_projection_order_with_metadata",
            "SELECT email, id FROM hive.raw.users",
            &["email", "id"],
            meta(),
        ),
        (
            "explicit_projection_order_two_tables",
            "SELECT o.amt, u.name, o.oid FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
            &["amt", "name", "oid"],
            meta(),
        ),
        (
            "explicit_then_qualified_star",
            "SELECT o.amt, u.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
            &["amt", "id", "name", "email"],
            meta(),
        ),
        (
            "two_qualified_stars_order_preserved",
            "SELECT o.*, u.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.id = o.uid",
            &["oid", "uid", "amt", "id", "name", "email"],
            meta(),
        ),
        (
            "duplicate_column_names",
            "SELECT id, id FROM hive.raw.users",
            &["id", "id"],
            meta(),
        ),
        (
            "duplicate_aliases",
            "SELECT a AS x, b AS x FROM t",
            &["x", "x"],
            trino(),
        ),
        (
            "col_index_position_zero",
            "SELECT count(*), a, sum(b) FROM t GROUP BY 2",
            &["_col0", "a", "_col2"],
            trino(),
        ),
        (
            "col_index_position_one",
            "SELECT a, count(*) FROM t GROUP BY 1",
            &["a", "_col1"],
            trino(),
        ),
        (
            "quoted_mixed_case_alias",
            r#"SELECT a AS "MiXeD Case", b AS "with""quote" FROM t"#,
            &["MiXeD Case", "with\"quote"],
            trino(),
        ),
        (
            "star_over_subquery_with_renames",
            "SELECT * FROM (SELECT id AS x, name AS y FROM hive.raw.users) s",
            &["x", "y"],
            meta(),
        ),
        (
            "star_over_cte_subset_reordered",
            "WITH c AS (SELECT email, id FROM hive.raw.users) SELECT * FROM c",
            &["email", "id"],
            meta(),
        ),
        (
            "union_leftmost_wins",
            "SELECT a AS l1, b AS l2 FROM t UNION ALL SELECT c, d FROM r",
            &["l1", "l2"],
            trino(),
        ),
        (
            "union_star_left_branch",
            "SELECT * FROM hive.raw.users UNION ALL SELECT oid, uid, amt FROM hive.raw.orders",
            &["id", "name", "email"],
            meta(),
        ),
        (
            "union_star_right_branch",
            "SELECT id, name, email FROM hive.raw.users UNION ALL SELECT * FROM hive.raw.orders",
            &["id", "name", "email"],
            meta(),
        ),
        (
            "create_view_column_list_count_mismatch",
            "CREATE VIEW c.s.v (x1, x2) AS SELECT a, b, c FROM t",
            &["x1", "x2"],
            trino(),
        ),
        (
            "insert_column_list_overrides_select_star",
            "INSERT INTO c.s.t (c1, c2, c3) SELECT * FROM hive.raw.users",
            &["c1", "c2", "c3"],
            meta(),
        ),
        (
            "count_star_not_a_star_projection",
            "SELECT count(*) FROM hive.raw.users",
            &["_col0"],
            trino(),
        ),
        (
            "count_star_alias",
            "SELECT count(*) AS n FROM hive.raw.users",
            &["n"],
            meta(),
        ),
        (
            "qualified_column_name",
            "SELECT u.id FROM hive.raw.users u",
            &["id"],
            meta(),
        ),
        (
            "empty_metadata_map",
            "SELECT * FROM hive.raw.users",
            &["*"],
            trino().schema(Vec::<(&str, Vec<&str>)>::new()),
        ),
        (
            "metadata_unrelated_table",
            "SELECT * FROM hive.raw.users",
            &["*"],
            trino().schema([("hive.raw.orders", vec!["oid"])]),
        ),
    ];
    for (name, sql, expected, opts) in cases {
        check(name, sql, expected, opts);
    }
}

#[test]
fn bughunt_col_index_after_star_format() {
    let got = columns("SELECT u.*, u.id + 1 FROM hive.raw.users u", &trino().schema(pc_meta())).unwrap();
    assert_eq!(got.len(), 4, "{got:?}");
    assert!(got[3] == "_col1" || got[3] == "_col3", "{got:?}");
}

#[test]
fn bughunt_mysql_backtick_alias() {
    check(
        "mysql_backtick_alias",
        "SELECT a AS `My Col`, b FROM t",
        &["My Col", "b"],
        options("mysql"),
    );
}

#[test]
fn bughunt_create_table_constraints_not_columns() {
    check(
        "create_table_constraints_not_columns",
        "CREATE TABLE t (id BIGINT, name VARCHAR(10), PRIMARY KEY (id), KEY idx_name (name))",
        &["id", "name"],
        options("mysql"),
    );
}

#[test]
fn bughunt_create_table_like_empty_metadata_entry() {
    check(
        "create_table_like_empty_metadata_entry",
        "CREATE TABLE c.s.t (LIKE c.s.other)",
        &[],
        trino().schema([("c.s.other", Vec::<&str>::new())]),
    );
}

#[test]
fn bughunt_create_table_like_ambiguous_suffix() {
    unsupported(
        "create_table_like_ambiguous_suffix",
        "CREATE TABLE c.s.t (LIKE other)",
        trino().schema([("a.b.other", vec!["a1"]), ("x.y.other", vec!["x1"])]),
    );
}

#[test]
fn bughunt_insert_default_values_without_column_list() {
    unsupported(
        "insert_default_values_without_column_list",
        "INSERT INTO c.s.t DEFAULT VALUES",
        options("postgres"),
    );
}

#[test]
fn bughunt_row_field_access() {
    let got = columns("SELECT t.c.f FROM c.s.t t", &trino()).unwrap();
    assert!(got == ["f"] || got == ["_col0"], "{got:?}");
}

#[test]
fn bughunt_with_recursive() {
    check(
        "with_recursive",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 3) SELECT n AS depth FROM r",
        &["depth"],
        options("postgres"),
    );
}

// --- polyglot_contract_test.go ---------------------------------------------

#[test]
fn native_output_columns_contract() {
    check(
        "unnamed, unknown qualified star, named",
        "SELECT 1, t.*, b FROM t",
        &["_col0", "*", "b"],
        trino(),
    );
    let ordered = || trino().schema([("t", vec!["z", "a"])]);
    check("schema order", "SELECT * FROM t", &["z", "a"], ordered());
    check(
        "empty schema means zero columns",
        "SELECT * FROM t",
        &[],
        trino().schema([("t", Vec::<&str>::new())]),
    );
}

#[test]
fn parse_columns_native_projection_order() {
    let opts = || trino().schema([("t", vec!["z", "a"])]);
    for (name, sql, expected) in [
        (
            "cte_reordered_projection",
            "WITH c AS (SELECT a, z FROM t) SELECT * FROM c",
            &["a", "z"][..],
        ),
        (
            "multiple_stars_and_expression",
            "SELECT t.*, 1, t.* FROM t",
            &["z", "a", "_col2", "z", "a"],
        ),
        (
            "duplicate_computed_alias",
            "SELECT 1 AS same, a AS same FROM t",
            &["same", "same"],
        ),
        (
            "explicit_synthetic_looking_alias",
            "SELECT a AS _col_0 FROM t",
            &["_col_0"],
        ),
    ] {
        check(name, sql, expected, opts());
    }
}

#[test]
fn parse_columns_union_by_name() {
    check(
        "union_by_name",
        "SELECT 1 AS a UNION ALL BY NAME SELECT 2 AS b",
        &["a", "b"],
        options("duckdb"),
    );
}

#[test]
fn native_parser_depth_and_eof_contract() {
    let deep = format!("SELECT {}1", "~ ".repeat(4000));
    let error = output_columns(&deep, &options("mysql")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    for sql in ["SELECT IF(IF(", "SELECT (", "SELECT ARRAY["] {
        let error = output_columns(sql, &options("mysql")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Parse, "truncated SQL {sql:?}: {error}");
    }
    check("usable after a guard failure", "SELECT 1", &["_col0"], options("mysql"));
}

#[test]
fn parse_columns_empty_metadata_scope() {
    let opts = || trino().schema([("empty", Vec::<&str>::new())]);
    for (sql, expected) in [
        ("SELECT 1, t.* FROM t", &["_col0", "*"][..]),
        ("SELECT 1, e.* FROM empty e", &["_col0"]),
        ("SELECT e.*, t.* FROM empty e CROSS JOIN t", &["*"]),
    ] {
        check(sql, sql, expected, opts());
    }
}
