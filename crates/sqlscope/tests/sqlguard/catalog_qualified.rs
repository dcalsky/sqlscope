//! Ported from sql-guard `catalog_qualified_api_test.go` and
//! `metadata_scope_test.go`.

use sqlscope::{
    apply_row_filter, column_origins, column_usages, inject_ctes, output_columns, referenced_columns, rewrite_tables,
    Clause, ColumnUsage, CteDef, TableRef, TableRewrite,
};

use crate::common::*;

const INTERACTION: &str = "iceberg.rda_launch_to_engage.interaction";
const HIVE_INTERACTION: &str = "hive.rda_launch_to_engage.interaction";

fn wraps(sql: &str, pattern: &str) -> usize {
    let out = apply_row_filter(sql, "tenant = 'alice'", &options("trino").table_patterns([pattern])).unwrap();
    count_literals(&out, "trino", "alice")
}

#[test]
fn apply_row_filter_catalog_qualified_regexp() {
    let pattern = r"^iceberg\.rda_launch_to_engage\.interaction$";
    assert_eq!(wraps(&format!("SELECT * FROM {INTERACTION}"), pattern), 1);
    assert_eq!(wraps(&format!("SELECT * FROM {HIVE_INTERACTION}"), pattern), 0);
}

#[test]
fn bind_ctes_catalog_qualified_table_names() {
    let out = inject_ctes(
        "SELECT f.interaction_id, u.name
		 FROM filtered f
		 JOIN hive.rda_launch_to_engage.users u ON f.user_id = u.user_id",
        &[CteDef::new(
            "filtered",
            "SELECT interaction_id, user_id
				FROM iceberg.rda_launch_to_engage.interaction
				WHERE active = TRUE",
        )],
        &options("trino"),
    )
    .unwrap();
    assert_eq!(
        out,
        "WITH filtered AS (SELECT interaction_id, user_id FROM iceberg.rda_launch_to_engage.interaction WHERE active = TRUE) SELECT f.interaction_id, u.name FROM filtered AS f JOIN hive.rda_launch_to_engage.users AS u ON f.user_id = u.user_id"
    );
}

#[test]
fn lineage_source_columns_catalog_qualified_table_names() {
    let got = column_origins(&format!("SELECT interaction_id FROM {INTERACTION}"), &options("trino")).unwrap();
    assert_eq!(got, map(&[(INTERACTION, &["interaction_id"])]));
}

/// `LineageSourceColumnsConcurrent` was a parallel variant of the same
/// operation; sqlscope has one entry point.
#[test]
fn lineage_source_columns_concurrent_catalog_qualified_table_names() {
    let sql =
        format!("SELECT interaction_id FROM {INTERACTION} UNION ALL SELECT interaction_id FROM {HIVE_INTERACTION}");
    let got = column_origins(&sql, &options("trino")).unwrap();
    assert_eq!(
        got,
        map(&[
            (INTERACTION, &["interaction_id"]),
            (HIVE_INTERACTION, &["interaction_id"])
        ])
    );
}

#[test]
fn parse_columns_catalog_qualified_table_names() {
    let opts = options("trino").schema([
        (INTERACTION, vec!["interaction_id", "status"]),
        (HIVE_INTERACTION, vec!["legacy_id"]),
    ]);
    let got = output_columns(&format!("SELECT * FROM {INTERACTION}"), &opts).unwrap();
    assert_eq!(got.unwrap(), ["interaction_id", "status"]);
}

const REFERENCES_SQL: &str = "
	SELECT
		iceberg.rda_launch_to_engage.interaction.interaction_id,
		hive.rda_launch_to_engage.interaction.legacy_id
	FROM iceberg.rda_launch_to_engage.interaction
	JOIN hive.rda_launch_to_engage.interaction
	  ON iceberg.rda_launch_to_engage.interaction.interaction_id =
	     hive.rda_launch_to_engage.interaction.legacy_id
	WHERE iceberg.rda_launch_to_engage.interaction.status = 'active'
";

#[test]
fn referenced_columns_catalog_qualified_table_names() {
    let got = referenced_columns(REFERENCES_SQL, &options("trino")).unwrap();
    assert_eq!(
        got,
        map(&[
            (INTERACTION, &["interaction_id", "status"]),
            (HIVE_INTERACTION, &["legacy_id"])
        ])
    );
}

#[test]
fn referenced_column_usages_catalog_qualified_table_names() {
    let got = column_usages(REFERENCES_SQL, &options("trino")).unwrap();
    let usage = |table: &str, column: &str, clause| ColumnUsage {
        table: table.into(),
        column: column.into(),
        clause,
    };
    let mut want = vec![
        usage(INTERACTION, "interaction_id", Clause::Select),
        usage(INTERACTION, "interaction_id", Clause::JoinOn),
        usage(INTERACTION, "status", Clause::Where),
        usage(HIVE_INTERACTION, "legacy_id", Clause::Select),
        usage(HIVE_INTERACTION, "legacy_id", Clause::JoinOn),
    ];
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn rewrite_table_references_catalog_qualified_table_names() {
    let plan = [TableRewrite::inline(
        "rda_launch_to_engage.interaction",
        TableRef::new("interaction")
            .with_schema("filtered")
            .with_catalog("lakehouse"),
    )];
    let opts = options("trino").strip_catalogs(["iceberg"]);
    let out = rewrite_tables(
        &format!("SELECT iceberg.rda_launch_to_engage.interaction.interaction_id FROM {INTERACTION}"),
        &plan,
        &opts,
    )
    .unwrap();
    assert_eq!(
        out,
        "SELECT interaction.interaction_id FROM (SELECT * FROM lakehouse.filtered.interaction) AS interaction"
    );
    let other = format!("SELECT * FROM {HIVE_INTERACTION}");
    assert_eq!(rewrite_tables(&other, &plan, &opts).unwrap(), other);
}

#[test]
fn parse_columns_unrelated_catalog_does_not_expand_star_from_other_table() {
    let opts = options("trino").schema([("foo", vec![]), ("other_table", vec!["a", "b", "c"])]);
    assert_eq!(
        output_columns("SELECT * FROM foo", &opts).unwrap().unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn parse_columns_star_without_table_metadata_falls_back_to_star() {
    let opts = options("trino").schema([("other_table", vec!["a", "b"])]);
    assert_eq!(output_columns("SELECT * FROM foo", &opts).unwrap().unwrap(), ["*"]);
}

#[test]
fn referenced_columns_unrelated_catalog_not_credited() {
    let opts = options("trino").schema([("foo", vec![]), ("other_table", vec!["a", "b", "c"])]);
    let got = referenced_columns("SELECT a, b FROM foo", &opts).unwrap();
    assert_eq!(got, map(&[("foo", &["a", "b"])]));
}
