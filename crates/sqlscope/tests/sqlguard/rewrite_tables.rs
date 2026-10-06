//! Ported from sql-guard `rewrite_tables_test.go` and
//! `rewrite_tables_reverse_test.go` (RewriteTableReferences -> rewrite_tables).

use sqlscope::{rewrite_tables, ErrorKind, Options, TableRef, TableRewrite, UnionRewrite};

use crate::common::*;

const UNION_SQL: &str = "(SELECT col1, col2 FROM cat.vsch.view1 UNION DISTINCT SELECT col1, col2 FROM cat.vsch.view2)";

fn view(table: &str) -> TableRef {
    TableRef::new(table).with_schema("vsch").with_catalog("cat")
}

fn inline() -> TableRewrite {
    TableRewrite::inline("myschema.mytable", view("view1"))
}

fn union_rewrite(alias: &str, columns: &[&str]) -> TableRewrite {
    TableRewrite::union(
        "myschema.mytable",
        UnionRewrite {
            table_alias: alias.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            branches: vec![view("view1"), view("view2")],
        },
    )
}

fn union() -> TableRewrite {
    union_rewrite("mytable", &["col1", "col2"])
}

fn same_name_specs() -> Vec<TableRewrite> {
    vec![
        TableRewrite::inline("s1.t", TableRef::new("view1").with_schema("vsch")),
        TableRewrite::inline("s2.t", TableRef::new("view2").with_schema("vsch")),
    ]
}

/// Rewrites and asserts that the output parses in `dialect`.
fn rewrite(sql: &str, rewrites: &[TableRewrite], dialect: &str, opts: Options) -> String {
    let out = rewrite_tables(sql, rewrites, &opts.dialect(crate::common::dialect(dialect)))
        .unwrap_or_else(|e| panic!("rewrite_tables({sql:?}): {e}"));
    parse_one(&out, dialect);
    out
}

fn trino(sql: &str, rewrites: &[TableRewrite]) -> String {
    rewrite(sql, rewrites, "trino", Options::new())
}

#[test]
fn rewrite_table_references_inline_literal_safe() {
    let out = trino(
        "SELECT col1 FROM myschema.mytable WHERE note = ' myschema.mytable ' AND txt = 'see vdm_rda.docs' AND spaced = 'a   b'",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable WHERE note = ' myschema.mytable ' AND txt = 'see vdm_rda.docs' AND spaced = 'a   b'"
    );
}

#[test]
fn rewrite_table_references_strip_match_catalog() {
    let out = rewrite(
        r#"SELECT col1 FROM "vdm_rda".myschema.mytable WHERE col1 = 'a'"#,
        &[inline()],
        "trino",
        Options::new().strip_catalogs(["vdm_rda"]),
    );
    assert_eq!(
        out,
        "SELECT col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable WHERE col1 = 'a'"
    );
}

#[test]
fn rewrite_table_references_union_derived_table_with_existing_with() {
    let out = trino(
        "WITH user_cte AS (SELECT 1 AS k) SELECT col1 FROM myschema.mytable WHERE col2 = 'a'",
        &[union()],
    );
    assert_eq!(
        out,
        format!("WITH user_cte AS (SELECT 1 AS k) SELECT col1 FROM {UNION_SQL} AS mytable WHERE col2 = 'a'")
    );
}

#[test]
fn rewrite_table_references_repeated_references() {
    let out = trino(
        "SELECT a.col1, b.col1 FROM myschema.mytable AS a JOIN myschema.mytable AS b ON a.id = b.id",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT a.col1, b.col1 FROM (SELECT * FROM cat.vsch.view1) AS a JOIN (SELECT * FROM cat.vsch.view1) AS b ON a.id = b.id"
    );
}

#[test]
fn rewrite_table_references_noop_preserves_input() {
    let input = "WITH mytable AS (SELECT 1 AS col1) SELECT col1 FROM mytable";
    assert_eq!(rewrite_tables(input, &[inline()], &options("trino")).unwrap(), input);
}

#[test]
fn rewrite_table_references_rejects_multiple_statements() {
    let error = rewrite_tables(
        "SELECT * FROM myschema.mytable; SELECT * FROM myschema.mytable",
        &[inline()],
        &options("trino"),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
}

/// The "no target" case of sql-guard cannot be expressed: a Rust
/// [`TableRewrite`] always has exactly one target.
#[test]
fn rewrite_table_references_rejects_invalid_plan() {
    let safe = || TableRef::new("t").with_schema("safe");
    let valid_union = |columns: Vec<&str>, branches: Vec<TableRef>| UnionRewrite {
        table_alias: "t".into(),
        columns: columns.into_iter().map(String::from).collect(),
        branches,
    };
    let cases = [
        (
            "unqualified match key",
            TableRewrite::inline("t", safe()),
            "must be schema.table",
        ),
        (
            "overqualified match key",
            TableRewrite::inline("cat.s.t", safe()),
            "must be schema.table",
        ),
        (
            "catalog without schema",
            TableRewrite::inline("s.t", TableRef::new("t").with_catalog("cat")),
            "catalog requires a schema",
        ),
        (
            "empty union column",
            TableRewrite::union("s.t", valid_union(vec![""], vec![safe()])),
            "column 0 must not be empty",
        ),
        (
            "union branch catalog without schema",
            TableRewrite::union(
                "s.t",
                valid_union(vec!["c"], vec![TableRef::new("t").with_catalog("cat")]),
            ),
            "catalog requires a schema",
        ),
    ];
    for (name, spec, contains) in cases {
        let error = rewrite_tables("SELECT * FROM s.t", &[spec], &Options::new()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{name}: {error}");
        assert!(error.message().contains(contains), "{name}: {error}");
    }
}

#[test]
fn rewrite_table_references_strips_catalog_qualified_column_refs() {
    let out = rewrite(
        "SELECT vdm_rda.myschema.mytable.col1 FROM vdm_rda.myschema.mytable",
        &[inline()],
        "trino",
        Options::new().strip_catalogs(["vdm_rda"]),
    );
    assert_eq!(
        out,
        "SELECT mytable.col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable"
    );
}

#[test]
fn rewrite_table_references_union_alias_keeps_qualified_refs_resolvable() {
    let out = trino(
        "SELECT myschema.mytable.col1 FROM myschema.mytable",
        &[union_rewrite("replacement", &["col1", "col2"])],
    );
    assert_eq!(out, format!("SELECT replacement.col1 FROM {UNION_SQL} AS replacement"));
}

#[test]
fn rewrite_table_references_preserves_postgres_dollar_quoted_strings() {
    let out = rewrite(
        "SELECT col1 FROM myschema.mytable WHERE note = $$not -- a comment$$",
        &[TableRewrite::inline(
            "myschema.mytable",
            TableRef::new("view1").with_schema("vsch"),
        )],
        "postgres",
        Options::new(),
    );
    assert_eq!(
        out,
        "SELECT col1 FROM (SELECT * FROM vsch.view1) AS mytable WHERE note = 'not -- a comment'"
    );
}

#[test]
fn rewrite_table_references_does_not_duplicate_aliases_for_same_bare_table_name() {
    let out = trino(
        "SELECT s1.t.id, s2.t.id FROM s1.t JOIN s2.t ON s1.t.id = s2.t.id",
        &same_name_specs(),
    );
    assert_eq!(
        out,
        "SELECT s1_t.id, s2_t.id FROM (SELECT * FROM vsch.view1) AS s1_t JOIN (SELECT * FROM vsch.view2) AS s2_t ON s1_t.id = s2_t.id"
    );
}

#[test]
fn rewrite_table_references_quotes_union_column_identifiers() {
    let out = trino(
        "SELECT col1 FROM myschema.mytable",
        &[union_rewrite("mytable", &["order id"])],
    );
    assert_eq!(
        out,
        r#"SELECT col1 FROM (SELECT "order id" FROM cat.vsch.view1 UNION DISTINCT SELECT "order id" FROM cat.vsch.view2) AS mytable"#
    );
}

#[test]
fn rewrite_table_references_uses_dialect_identifier_quoting_for_targets() {
    let out = rewrite(
        "SELECT col1 FROM myschema.mytable",
        &[TableRewrite::inline(
            "myschema.mytable",
            TableRef::new("view-1").with_schema("vsch"),
        )],
        "mysql",
        Options::new(),
    );
    assert_eq!(out, "SELECT col1 FROM (SELECT * FROM vsch.`view-1`) AS mytable");
}

#[test]
fn rewrite_table_references_cte_does_not_shadow_qualified_physical_table() {
    let out = trino(
        "WITH mytable AS (SELECT col1 FROM other_table)
         SELECT mytable.col1, myschema.mytable.col1
         FROM mytable
         JOIN myschema.mytable ON mytable.col1 = myschema.mytable.col1",
        &[TableRewrite::inline(
            "myschema.mytable",
            TableRef::new("view1").with_schema("vsch"),
        )],
    );
    assert_eq!(
        out,
        "WITH mytable AS (SELECT col1 FROM other_table) SELECT mytable.col1, myschema_mytable.col1 FROM mytable JOIN (SELECT * FROM vsch.view1) AS myschema_mytable ON mytable.col1 = myschema_mytable.col1"
    );
}

#[test]
fn rewrite_table_references_unused_cte_does_not_block_bare_rebind() {
    let out = trino(
        "WITH mytable AS (SELECT 999 AS col1)
         SELECT mytable.col1 FROM myschema.mytable WHERE mytable.col1 = 'a'",
        &[inline()],
    );
    assert_eq!(
        out,
        "WITH mytable AS (SELECT 999 AS col1) SELECT myschema_mytable.col1 FROM (SELECT * FROM cat.vsch.view1) AS myschema_mytable WHERE myschema_mytable.col1 = 'a'"
    );
}

#[test]
fn rewrite_table_references_ambiguous_outer_bare_qualifier_does_not_break_inner_alias() {
    let out = trino(
        "SELECT (SELECT t.id FROM other_table t) AS inner_id, s1.t.id
         FROM s1.t JOIN s2.t ON s1.t.id = s2.t.id",
        &same_name_specs(),
    );
    assert_eq!(
        out,
        "SELECT (SELECT t.id FROM other_table AS t) AS inner_id, s1_t.id FROM (SELECT * FROM vsch.view1) AS s1_t JOIN (SELECT * FROM vsch.view2) AS s2_t ON s1_t.id = s2_t.id"
    );
}

#[test]
fn rewrite_table_references_nested_alias_does_not_capture_outer_rebind() {
    let out = trino(
        "SELECT (SELECT mytable.id FROM other_table mytable) AS inner_id, myschema.mytable.id
         FROM myschema.mytable",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT (SELECT mytable.id FROM other_table AS mytable) AS inner_id, mytable.id FROM (SELECT * FROM cat.vsch.view1) AS mytable"
    );
}

#[test]
fn rewrite_table_references_projection_alias_does_not_rename_table_alias() {
    let out = trino(
        "SELECT mytable.col1 AS x, 1 AS mytable FROM myschema.mytable",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT mytable.col1 AS x, 1 AS mytable FROM (SELECT * FROM cat.vsch.view1) AS mytable"
    );
}

#[test]
fn rewrite_table_references_nested_alias_does_not_rename_outer_table_alias() {
    let out = trino(
        "SELECT mytable.col1, (SELECT 1 FROM other_table mytable) AS inner_value FROM myschema.mytable",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT mytable.col1, (SELECT 1 FROM other_table AS mytable) AS inner_value FROM (SELECT * FROM cat.vsch.view1) AS mytable"
    );
}

#[test]
fn rewrite_table_references_union_alias_keeps_bare_refs_resolvable() {
    let out = trino(
        "SELECT mytable.col1 FROM myschema.mytable",
        &[union_rewrite("replacement", &["col1", "col2"])],
    );
    assert_eq!(out, format!("SELECT replacement.col1 FROM {UNION_SQL} AS replacement"));
}

#[test]
fn rewrite_table_references_qualified_star_rebinds_to_derived_alias() {
    let want = "SELECT mytable.* FROM (SELECT * FROM cat.vsch.view1) AS mytable";
    assert_eq!(
        trino("SELECT myschema.mytable.* FROM myschema.mytable", &[inline()]),
        want
    );
    let out = rewrite(
        "SELECT vdm_rda.myschema.mytable.* FROM vdm_rda.myschema.mytable",
        &[inline()],
        "trino",
        Options::new().strip_catalogs(["vdm_rda"]),
    );
    assert_eq!(out, want);
}

#[test]
fn rewrite_table_references_union_all_branch_qualified_column_refs_rebind() {
    let out = trino(
        "SELECT myschema.mytable.col1 FROM myschema.mytable UNION ALL SELECT col1 FROM other_table",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT mytable.col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable UNION ALL SELECT col1 FROM other_table"
    );
}

#[test]
fn rewrite_table_references_union_all_branch_qualified_star_rebinds() {
    let out = trino(
        "SELECT myschema.mytable.* FROM myschema.mytable UNION ALL SELECT col1 FROM other_table",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT mytable.* FROM (SELECT * FROM cat.vsch.view1) AS mytable UNION ALL SELECT col1 FROM other_table"
    );
}

#[test]
fn rewrite_table_references_except_branch_where_clause_rebinds() {
    let out = trino(
        "SELECT col1 FROM myschema.mytable WHERE myschema.mytable.col2 > 1 EXCEPT SELECT col1 FROM other_table",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable WHERE mytable.col2 > 1 EXCEPT SELECT col1 FROM other_table"
    );
}

#[test]
fn rewrite_table_references_union_all_right_branch_qualified_column_refs_rebind() {
    let out = trino(
        "SELECT col1 FROM other_table UNION ALL SELECT myschema.mytable.col1 FROM myschema.mytable",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT col1 FROM other_table UNION ALL SELECT mytable.col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable"
    );
}

#[test]
fn rewrite_table_references_union_renamed_bare_star_in_set_operation_branch_rebinds() {
    let out = trino(
        "SELECT mytable.* FROM myschema.mytable UNION ALL SELECT col1 FROM other_table",
        &[union_rewrite("replacement", &["col1", "col2"])],
    );
    assert_eq!(
        out,
        format!("SELECT replacement.* FROM {UNION_SQL} AS replacement UNION ALL SELECT col1 FROM other_table")
    );
}

#[test]
fn rewrite_table_references_bare_rebind_is_scoped_to_each_query() {
    for (name, sql, want) in [
        (
            "subquery in where",
            "SELECT t.a FROM s1.t WHERE t.b IN (SELECT t.c FROM s2.t)",
            "SELECT s1_t.a FROM (SELECT * FROM vsch.view1) AS s1_t WHERE s1_t.b IN (SELECT s2_t.c FROM (SELECT * FROM vsch.view2) AS s2_t)",
        ),
        (
            "subquery in projection",
            "SELECT (SELECT t.c FROM s2.t) AS v, t.a FROM s1.t",
            "SELECT (SELECT s2_t.c FROM (SELECT * FROM vsch.view2) AS s2_t) AS v, s1_t.a FROM (SELECT * FROM vsch.view1) AS s1_t",
        ),
    ] {
        assert_eq!(trino(sql, &same_name_specs()), want, "{name}");
    }
}

#[test]
fn rewrite_table_references_quoted_dot_star_is_one_identifier() {
    let out = trino(r#"SELECT "s1.t".*, s1.t.a FROM "s1.t", s1.t"#, &same_name_specs());
    assert_eq!(
        out,
        r#"SELECT "s1.t".*, t.a FROM "s1.t", (SELECT * FROM vsch.view1) AS t"#
    );
}

#[test]
fn rewrite_table_references_row_field_access_follows_renamed_table() {
    let out = trino(
        "SELECT mytable.col1.f FROM myschema.mytable",
        &[union_rewrite("replacement", &["col1", "col2"])],
    );
    assert_eq!(
        out,
        format!("SELECT replacement.col1.f FROM {UNION_SQL} AS replacement")
    );
}

#[test]
fn rewrite_table_references_union_alias_avoids_existing_derived_alias() {
    let out = trino(
        "SELECT mytable.col1 FROM myschema.mytable, (SELECT 1 AS c) AS replacement",
        &[union_rewrite("replacement", &["col1", "col2"])],
    );
    assert_eq!(
        out,
        format!("SELECT myschema_mytable.col1 FROM {UNION_SQL} AS myschema_mytable, (SELECT 1 AS c) AS replacement")
    );
}

#[test]
fn rewrite_table_references_preserves_alias_column_list() {
    let out = trino("SELECT a.x FROM myschema.mytable AS a (x)", &[inline()]);
    assert_eq!(out, "SELECT a.x FROM (SELECT * FROM cat.vsch.view1) AS a(x)");
}

#[test]
fn rewrite_table_references_preserves_table_sample() {
    let out = trino(
        "SELECT col1 FROM myschema.mytable TABLESAMPLE BERNOULLI (10)",
        &[inline()],
    );
    assert_eq!(
        out,
        "SELECT col1 FROM (SELECT * FROM cat.vsch.view1 TABLESAMPLE BERNOULLI (10)) AS mytable"
    );
}

#[test]
fn rewrite_table_references_quotes_reserved_identifiers() {
    let cases = [
        (
            "inline target",
            TableRewrite::inline("myschema.mytable", TableRef::new("order").with_schema("vsch")),
            r#"SELECT col1 FROM (SELECT * FROM vsch."order") AS mytable"#.to_owned(),
        ),
        (
            "union column",
            union_rewrite("mytable", &["order"]),
            r#"SELECT col1 FROM (SELECT "order" FROM cat.vsch.view1 UNION DISTINCT SELECT "order" FROM cat.vsch.view2) AS mytable"#.to_owned(),
        ),
        (
            "union alias",
            union_rewrite("order", &["col1", "col2"]),
            format!(r#"SELECT col1 FROM {UNION_SQL} AS "order""#),
        ),
    ];
    for (name, spec, want) in cases {
        assert_eq!(trino("SELECT col1 FROM myschema.mytable", &[spec]), want, "{name}");
    }
}

#[test]
fn rewrite_table_references_preserves_mixed_case_target_identifiers() {
    let out = rewrite(
        "SELECT * FROM s.t",
        &[TableRewrite::inline(
            "s.t",
            TableRef::new("TargetTable")
                .with_schema("TargetSchema")
                .with_catalog("TargetCatalog"),
        )],
        "postgres",
        Options::new(),
    );
    assert_eq!(
        out,
        r#"SELECT * FROM (SELECT * FROM "TargetCatalog"."TargetSchema"."TargetTable") AS t"#
    );
}

#[test]
fn rewrite_table_references_case_distinct_postgres_tables_get_distinct_aliases() {
    let out = rewrite(
        r#"SELECT myschema."MyTable".a, myschema.mytable.b FROM myschema."MyTable", myschema.mytable"#,
        &[TableRewrite::inline(
            "myschema.mytable",
            TableRef::new("view1").with_schema("vsch"),
        )],
        "postgres",
        Options::new(),
    );
    assert_eq!(
        out,
        r#"SELECT "myschema_MyTable".a, mytable_2.b FROM (SELECT * FROM vsch.view1) AS "myschema_MyTable", (SELECT * FROM vsch.view1) AS mytable_2"#
    );
}

#[test]
fn rewrite_table_references_unnest_alias_occupies_relation_name() {
    let out = trino(
        "SELECT t.x, mytable.col1 FROM myschema.mytable, UNNEST(ARRAY[1]) AS t (x)",
        &[union_rewrite("t", &["col1", "col2"])],
    );
    assert_eq!(
        out,
        format!("SELECT t.x, myschema_mytable.col1 FROM {UNION_SQL} AS myschema_mytable, UNNEST(ARRAY[1]) AS t(x)")
    );
}

#[test]
fn rewrite_table_references_union_keeps_explicit_alias() {
    let out = trino("SELECT a.col1 FROM myschema.mytable AS a", &[union()]);
    assert_eq!(out, format!("SELECT a.col1 FROM {UNION_SQL} AS a"));
}
