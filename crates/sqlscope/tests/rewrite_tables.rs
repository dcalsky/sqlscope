#![allow(clippy::type_complexity)]

mod common;

use common::*;
use sqlscope::{rewrite_tables, ErrorKind, Options, TableRef, TableRewrite, UnionRewrite};

fn cat_vsch(table: &str) -> TableRef {
    TableRef::new(table).with_schema("vsch").with_catalog("cat")
}

fn inline() -> TableRewrite {
    TableRewrite::inline("myschema.mytable", cat_vsch("view1"))
}

fn union_with(alias: &str, columns: &[&str]) -> TableRewrite {
    TableRewrite::union(
        "myschema.mytable",
        UnionRewrite {
            table_alias: alias.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            branches: vec![cat_vsch("view1"), cat_vsch("view2")],
        },
    )
}

fn union() -> TableRewrite {
    union_with("mytable", &["col1", "col2"])
}

fn same_name_specs() -> Vec<TableRewrite> {
    vec![
        TableRewrite::inline("s1.t", TableRef::new("view1").with_schema("vsch")),
        TableRewrite::inline("s2.t", TableRef::new("view2").with_schema("vsch")),
    ]
}

fn check(sql: &str, rewrites: &[TableRewrite], opts: Options, want: &str) {
    let dialect = opts.clone();
    let out = rewrite_tables(sql, rewrites, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let _ = dialect;
    parse_one(&out, "trino");
    assert_eq!(out, want, "\n in: {sql}");
}

const UNION_SQL: &str = "(SELECT col1, col2 FROM cat.vsch.view1 UNION DISTINCT SELECT col1, col2 FROM cat.vsch.view2)";

#[test]
fn inline_rewrite_keeps_literals() {
    check(
        "SELECT col1 FROM myschema.mytable WHERE note = ' myschema.mytable ' AND txt = 'see vdm_rda.docs' AND spaced = 'a   b'",
        &[inline()],
        Options::new(),
        "SELECT col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable WHERE note = ' myschema.mytable ' AND txt = 'see vdm_rda.docs' AND spaced = 'a   b'",
    );
}

#[test]
fn strip_catalogs() {
    check(
        r#"SELECT col1 FROM "vdm_rda".myschema.mytable WHERE col1 = 'a'"#,
        &[inline()],
        Options::new().strip_catalogs(["vdm_rda"]),
        "SELECT col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable WHERE col1 = 'a'",
    );
    check(
        "SELECT vdm_rda.myschema.mytable.col1 FROM vdm_rda.myschema.mytable",
        &[inline()],
        Options::new().strip_catalogs(["vdm_rda"]),
        "SELECT mytable.col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable",
    );
    // An unlisted catalog never matches.
    let sql = "SELECT * FROM other.myschema.mytable";
    assert_eq!(
        rewrite_tables(sql, &[inline()], &Options::new().strip_catalogs(["vdm_rda"])).unwrap(),
        sql
    );
}

#[test]
fn union_rewrite_with_existing_with() {
    check(
        "WITH user_cte AS (SELECT 1 AS k) SELECT col1 FROM myschema.mytable WHERE col2 = 'a'",
        &[union()],
        Options::new(),
        &format!("WITH user_cte AS (SELECT 1 AS k) SELECT col1 FROM {UNION_SQL} AS mytable WHERE col2 = 'a'"),
    );
}

#[test]
fn repeated_references_keep_aliases() {
    check(
        "SELECT a.col1, b.col1 FROM myschema.mytable AS a JOIN myschema.mytable AS b ON a.id = b.id",
        &[inline()],
        Options::new(),
        "SELECT a.col1, b.col1 FROM (SELECT * FROM cat.vsch.view1) AS a JOIN (SELECT * FROM cat.vsch.view1) AS b ON a.id = b.id",
    );
    check(
        "SELECT a.col1 FROM myschema.mytable AS a",
        &[union()],
        Options::new(),
        &format!("SELECT a.col1 FROM {UNION_SQL} AS a"),
    );
}

#[test]
fn no_match_preserves_input() {
    let sql = "WITH mytable AS (SELECT 1 AS col1) SELECT col1 FROM mytable";
    assert_eq!(rewrite_tables(sql, &[inline()], &Options::new()).unwrap(), sql);
    assert_eq!(rewrite_tables("not sql", &[], &Options::new()).unwrap(), "not sql");
}

#[test]
fn rejects_multiple_statements() {
    let err = rewrite_tables("SELECT * FROM myschema.mytable; SELECT 1", &[inline()], &Options::new()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}

#[test]
fn rejects_invalid_plans() {
    let valid_union = || UnionRewrite {
        table_alias: "t".into(),
        columns: vec!["c".into()],
        branches: vec![TableRef::new("t").with_schema("safe")],
    };
    let safe = TableRef::new("t").with_schema("safe");
    let cases: Vec<(TableRewrite, &str)> = vec![
        (TableRewrite::inline("t", safe.clone()), "must be schema.table"),
        (TableRewrite::inline("cat.s.t", safe.clone()), "must be schema.table"),
        (
            TableRewrite::inline("s.t", TableRef::new("t").with_catalog("cat")),
            "catalog requires a schema",
        ),
        (
            TableRewrite::inline("s.t", TableRef::new(" ")),
            "target table must not be empty",
        ),
        (
            TableRewrite::union(
                "s.t",
                UnionRewrite {
                    columns: vec!["".into()],
                    ..valid_union()
                },
            ),
            "column 0 must not be empty",
        ),
        (
            TableRewrite::union(
                "s.t",
                UnionRewrite {
                    columns: vec![],
                    ..valid_union()
                },
            ),
            "at least one column",
        ),
        (
            TableRewrite::union(
                "s.t",
                UnionRewrite {
                    table_alias: "".into(),
                    ..valid_union()
                },
            ),
            "requires a table alias",
        ),
        (
            TableRewrite::union(
                "s.t",
                UnionRewrite {
                    branches: vec![],
                    ..valid_union()
                },
            ),
            "at least one branch",
        ),
        (
            TableRewrite::union(
                "s.t",
                UnionRewrite {
                    branches: vec![TableRef::new("t").with_catalog("cat")],
                    ..valid_union()
                },
            ),
            "catalog requires a schema",
        ),
    ];
    for (spec, message) in cases {
        let err = rewrite_tables("SELECT * FROM s.t", &[spec], &Options::new()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidArgument);
        assert!(err.to_string().contains(message), "{err} lacks {message:?}");
    }
    let dup = [
        TableRewrite::inline("s.t", safe.clone()),
        TableRewrite::inline("S.T", safe),
    ];
    assert!(rewrite_tables("SELECT 1", &dup, &Options::new())
        .unwrap_err()
        .to_string()
        .contains("duplicate"));
}

#[test]
fn union_alias_rebinds_qualified_and_bare_references() {
    let replacement = union_with("replacement", &["col1", "col2"]);
    let want = format!("SELECT replacement.col1 FROM {UNION_SQL} AS replacement");
    check(
        "SELECT myschema.mytable.col1 FROM myschema.mytable",
        std::slice::from_ref(&replacement),
        Options::new(),
        &want,
    );
    check(
        "SELECT mytable.col1 FROM myschema.mytable",
        std::slice::from_ref(&replacement),
        Options::new(),
        &want,
    );
    check(
        "SELECT mytable.col1.f FROM myschema.mytable",
        std::slice::from_ref(&replacement),
        Options::new(),
        &format!("SELECT replacement.col1.f FROM {UNION_SQL} AS replacement"),
    );
    check(
        "SELECT mytable.* FROM myschema.mytable UNION ALL SELECT col1 FROM other_table",
        &[replacement],
        Options::new(),
        &format!("SELECT replacement.* FROM {UNION_SQL} AS replacement UNION ALL SELECT col1 FROM other_table"),
    );
}

#[test]
fn postgres_dollar_quoted_strings() {
    check(
        "SELECT col1 FROM myschema.mytable WHERE note = $$not -- a comment$$",
        &[TableRewrite::inline(
            "myschema.mytable",
            TableRef::new("view1").with_schema("vsch"),
        )],
        options("postgres"),
        "SELECT col1 FROM (SELECT * FROM vsch.view1) AS mytable WHERE note = 'not -- a comment'",
    );
}

#[test]
fn same_bare_name_gets_distinct_aliases() {
    check(
        "SELECT s1.t.id, s2.t.id FROM s1.t JOIN s2.t ON s1.t.id = s2.t.id",
        &same_name_specs(),
        Options::new(),
        "SELECT s1_t.id, s2_t.id FROM (SELECT * FROM vsch.view1) AS s1_t JOIN (SELECT * FROM vsch.view2) AS s2_t ON s1_t.id = s2_t.id",
    );
    check(
        "SELECT (SELECT t.id FROM other_table t) AS inner_id, s1.t.id FROM s1.t JOIN s2.t ON s1.t.id = s2.t.id",
        &same_name_specs(),
        Options::new(),
        "SELECT (SELECT t.id FROM other_table AS t) AS inner_id, s1_t.id FROM (SELECT * FROM vsch.view1) AS s1_t JOIN (SELECT * FROM vsch.view2) AS s2_t ON s1_t.id = s2_t.id",
    );
}

#[test]
fn bare_rebinding_is_scoped_to_each_query() {
    check(
        "SELECT t.a FROM s1.t WHERE t.b IN (SELECT t.c FROM s2.t)",
        &same_name_specs(),
        Options::new(),
        "SELECT s1_t.a FROM (SELECT * FROM vsch.view1) AS s1_t WHERE s1_t.b IN (SELECT s2_t.c FROM (SELECT * FROM vsch.view2) AS s2_t)",
    );
    check(
        "SELECT (SELECT t.c FROM s2.t) AS v, t.a FROM s1.t",
        &same_name_specs(),
        Options::new(),
        "SELECT (SELECT s2_t.c FROM (SELECT * FROM vsch.view2) AS s2_t) AS v, s1_t.a FROM (SELECT * FROM vsch.view1) AS s1_t",
    );
}

#[test]
fn quoting_of_targets() {
    check(
        "SELECT col1 FROM myschema.mytable",
        &[union_with("mytable", &["order id"])],
        Options::new(),
        r#"SELECT col1 FROM (SELECT "order id" FROM cat.vsch.view1 UNION DISTINCT SELECT "order id" FROM cat.vsch.view2) AS mytable"#,
    );
    check(
        "SELECT col1 FROM myschema.mytable",
        &[TableRewrite::inline(
            "myschema.mytable",
            TableRef::new("view-1").with_schema("vsch"),
        )],
        options("mysql"),
        "SELECT col1 FROM (SELECT * FROM vsch.`view-1`) AS mytable",
    );
    check(
        "SELECT col1 FROM myschema.mytable",
        &[TableRewrite::inline(
            "myschema.mytable",
            TableRef::new("order").with_schema("vsch"),
        )],
        Options::new(),
        r#"SELECT col1 FROM (SELECT * FROM vsch."order") AS mytable"#,
    );
    check(
        "SELECT col1 FROM myschema.mytable",
        &[union_with("mytable", &["order"])],
        Options::new(),
        r#"SELECT col1 FROM (SELECT "order" FROM cat.vsch.view1 UNION DISTINCT SELECT "order" FROM cat.vsch.view2) AS mytable"#,
    );
    check(
        "SELECT col1 FROM myschema.mytable",
        &[union_with("order", &["col1", "col2"])],
        Options::new(),
        &format!(r#"SELECT col1 FROM {UNION_SQL} AS "order""#),
    );
    check(
        "SELECT * FROM s.t",
        &[TableRewrite::inline(
            "s.t",
            TableRef::new("TargetTable")
                .with_schema("TargetSchema")
                .with_catalog("TargetCatalog"),
        )],
        options("postgres"),
        r#"SELECT * FROM (SELECT * FROM "TargetCatalog"."TargetSchema"."TargetTable") AS t"#,
    );
}

#[test]
fn cte_scoping() {
    let target = [TableRewrite::inline(
        "myschema.mytable",
        TableRef::new("view1").with_schema("vsch"),
    )];
    check(
        "WITH mytable AS (SELECT col1 FROM other_table) SELECT mytable.col1, myschema.mytable.col1 FROM mytable JOIN myschema.mytable ON mytable.col1 = myschema.mytable.col1",
        &target,
        Options::new(),
        "WITH mytable AS (SELECT col1 FROM other_table) SELECT mytable.col1, myschema_mytable.col1 FROM mytable JOIN (SELECT * FROM vsch.view1) AS myschema_mytable ON mytable.col1 = myschema_mytable.col1",
    );
    check(
        "WITH mytable AS (SELECT 999 AS col1) SELECT mytable.col1 FROM myschema.mytable WHERE mytable.col1 = 'a'",
        &[inline()],
        Options::new(),
        "WITH mytable AS (SELECT 999 AS col1) SELECT myschema_mytable.col1 FROM (SELECT * FROM cat.vsch.view1) AS myschema_mytable WHERE myschema_mytable.col1 = 'a'",
    );
}

#[test]
fn nested_and_projection_aliases_do_not_capture_rebinding() {
    check(
        "SELECT (SELECT mytable.id FROM other_table mytable) AS inner_id, myschema.mytable.id FROM myschema.mytable",
        &[inline()],
        Options::new(),
        "SELECT (SELECT mytable.id FROM other_table AS mytable) AS inner_id, mytable.id FROM (SELECT * FROM cat.vsch.view1) AS mytable",
    );
    check(
        "SELECT mytable.col1 AS x, 1 AS mytable FROM myschema.mytable",
        &[inline()],
        Options::new(),
        "SELECT mytable.col1 AS x, 1 AS mytable FROM (SELECT * FROM cat.vsch.view1) AS mytable",
    );
    check(
        "SELECT mytable.col1, (SELECT 1 FROM other_table mytable) AS inner_value FROM myschema.mytable",
        &[inline()],
        Options::new(),
        "SELECT mytable.col1, (SELECT 1 FROM other_table AS mytable) AS inner_value FROM (SELECT * FROM cat.vsch.view1) AS mytable",
    );
}

#[test]
fn qualified_stars_and_set_operation_branches() {
    let want = "SELECT mytable.* FROM (SELECT * FROM cat.vsch.view1) AS mytable";
    check(
        "SELECT myschema.mytable.* FROM myschema.mytable",
        &[inline()],
        Options::new(),
        want,
    );
    check(
        "SELECT vdm_rda.myschema.mytable.* FROM vdm_rda.myschema.mytable",
        &[inline()],
        Options::new().strip_catalogs(["vdm_rda"]),
        want,
    );
    check(
        "SELECT myschema.mytable.col1 FROM myschema.mytable UNION ALL SELECT col1 FROM other_table",
        &[inline()],
        Options::new(),
        "SELECT mytable.col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable UNION ALL SELECT col1 FROM other_table",
    );
    check(
        "SELECT myschema.mytable.* FROM myschema.mytable UNION ALL SELECT col1 FROM other_table",
        &[inline()],
        Options::new(),
        "SELECT mytable.* FROM (SELECT * FROM cat.vsch.view1) AS mytable UNION ALL SELECT col1 FROM other_table",
    );
    check(
        "SELECT col1 FROM myschema.mytable WHERE myschema.mytable.col2 > 1 EXCEPT SELECT col1 FROM other_table",
        &[inline()],
        Options::new(),
        "SELECT col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable WHERE mytable.col2 > 1 EXCEPT SELECT col1 FROM other_table",
    );
    check(
        "SELECT col1 FROM other_table UNION ALL SELECT myschema.mytable.col1 FROM myschema.mytable",
        &[inline()],
        Options::new(),
        "SELECT col1 FROM other_table UNION ALL SELECT mytable.col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable",
    );
    check(
        r#"SELECT "s1.t".*, s1.t.a FROM "s1.t", s1.t"#,
        &same_name_specs(),
        Options::new(),
        r#"SELECT "s1.t".*, t.a FROM "s1.t", (SELECT * FROM vsch.view1) AS t"#,
    );
}

#[test]
fn alias_collisions_with_sibling_relations() {
    let replacement = union_with("replacement", &["col1", "col2"]);
    check(
        "SELECT mytable.col1 FROM myschema.mytable, (SELECT 1 AS c) AS replacement",
        &[replacement],
        Options::new(),
        &format!("SELECT myschema_mytable.col1 FROM {UNION_SQL} AS myschema_mytable, (SELECT 1 AS c) AS replacement"),
    );
    check(
        "SELECT t.x, mytable.col1 FROM myschema.mytable, UNNEST(ARRAY[1]) AS t (x)",
        &[union_with("t", &["col1", "col2"])],
        Options::new(),
        &format!("SELECT t.x, myschema_mytable.col1 FROM {UNION_SQL} AS myschema_mytable, UNNEST(ARRAY[1]) AS t(x)"),
    );
    check(
        r#"SELECT myschema."MyTable".a, myschema.mytable.b FROM myschema."MyTable", myschema.mytable"#,
        &[TableRewrite::inline(
            "myschema.mytable",
            TableRef::new("view1").with_schema("vsch"),
        )],
        options("postgres"),
        r#"SELECT "myschema_MyTable".a, mytable_2.b FROM (SELECT * FROM vsch.view1) AS "myschema_MyTable", (SELECT * FROM vsch.view1) AS mytable_2"#,
    );
}

#[test]
fn alias_column_list_and_tablesample_are_preserved() {
    check(
        "SELECT a.x FROM myschema.mytable AS a (x)",
        &[inline()],
        Options::new(),
        "SELECT a.x FROM (SELECT * FROM cat.vsch.view1) AS a(x)",
    );
    check(
        "SELECT col1 FROM myschema.mytable TABLESAMPLE BERNOULLI (10)",
        &[inline()],
        Options::new(),
        "SELECT col1 FROM (SELECT * FROM cat.vsch.view1 TABLESAMPLE BERNOULLI (10)) AS mytable",
    );
}

#[test]
fn catalog_qualified_tables() {
    let specs = [TableRewrite::inline(
        "rda_launch_to_engage.interaction",
        TableRef::new("interaction")
            .with_schema("filtered")
            .with_catalog("lakehouse"),
    )];
    let opts = Options::new().strip_catalogs(["iceberg"]);
    check(
        "SELECT iceberg.rda_launch_to_engage.interaction.interaction_id FROM iceberg.rda_launch_to_engage.interaction",
        &specs,
        opts.clone(),
        "SELECT interaction.interaction_id FROM (SELECT * FROM lakehouse.filtered.interaction) AS interaction",
    );
    let other = "SELECT * FROM hive.rda_launch_to_engage.interaction";
    assert_eq!(rewrite_tables(other, &specs, &opts).unwrap(), other);
}
