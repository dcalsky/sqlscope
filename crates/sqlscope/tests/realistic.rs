//! Scenarios modelled on queries that real applications send: multi-tenant
//! dashboards, dbt-style models, BigQuery/Snowflake/Postgres idioms and ORM
//! naming conventions.
#![allow(clippy::type_complexity)]

mod common;

use common::*;
use sqlscope::{
    apply_row_filter, column_origins, inject_ctes, output_columns, referenced_columns, rewrite_tables, CteDef,
    ErrorKind, Options, TableRef, TableRewrite, UnionRewrite,
};

const TENANT: &str = "tenant_id = 7";

fn shop(dialect: &str) -> Options {
    options(dialect).schema([
        ("orders", vec!["id", "customer_id", "total", "status", "created_at"]),
        ("customers", vec!["id", "name", "email"]),
        ("order_items", vec!["order_id", "sku", "qty", "price"]),
    ])
}

fn filter(dialect: &str, sql: &str) -> String {
    let out = apply_row_filter(sql, TENANT, &options(dialect)).unwrap_or_else(|e| panic!("{sql}: {e}"));
    parse_one(&out, dialect);
    out
}

fn origins(sql: &str, opts: &Options) -> std::collections::BTreeMap<String, Vec<String>> {
    column_origins(sql, opts).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn outputs(sql: &str, opts: &Options) -> Vec<String> {
    output_columns(sql, opts)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .unwrap_or_else(|| panic!("{sql}: no output columns"))
}

// ---------------------------------------------------------------------------
// apply_row_filter: tenant isolation must reach every table a query reads
// ---------------------------------------------------------------------------

#[test]
fn row_filter_reaches_subqueries_in_every_clause() {
    // A dashboard query that reads tables from SELECT, HAVING and ORDER BY
    // subqueries; each of them must be filtered or data leaks across tenants.
    let sql = "SELECT o.customer_id, count(*) AS n, \
                      (SELECT max(c.name) FROM customers c WHERE c.id = o.customer_id) AS name \
               FROM orders o \
               GROUP BY o.customer_id \
               HAVING count(*) > (SELECT avg(cnt) FROM order_stats) \
               ORDER BY (SELECT max(e.ts) FROM events e WHERE e.customer_id = o.customer_id)";
    let out = filter("postgres", sql);
    for table in ["orders", "customers", "order_stats", "events"] {
        assert!(
            out.contains(&format!("FROM {table} WHERE tenant_id = 7")),
            "{table} is not filtered:\n{out}"
        );
    }
}

#[test]
fn row_filter_keeps_the_callers_own_tenant_condition() {
    // The original WHERE must survive next to the injected filter; a user
    // asking for another tenant gets no rows rather than that tenant's rows.
    let out = filter("postgres", "SELECT * FROM orders WHERE tenant_id = 8");
    assert_eq!(
        out,
        "SELECT * FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders WHERE tenant_id = 8"
    );
}

#[test]
fn row_filter_does_not_filter_tables_inside_the_predicate() {
    // A membership predicate reads its own table; filtering that table by the
    // predicate again would recurse and is not what the caller asked for.
    let out = apply_row_filter(
        "SELECT * FROM orders",
        "tenant_id IN (SELECT tenant_id FROM memberships WHERE user_id = 3)",
        &options("postgres"),
    )
    .unwrap();
    assert_eq!(
        out,
        "SELECT * FROM (SELECT * FROM orders WHERE tenant_id IN (SELECT tenant_id FROM memberships WHERE user_id = 3)) AS orders"
    );
}

#[test]
fn row_filter_keeps_table_hints_and_modifiers_on_the_physical_table() {
    let cases = [
        ("postgres", "SELECT * FROM ONLY orders", "FROM ONLY orders WHERE"),
        (
            "postgres",
            "SELECT * FROM orders TABLESAMPLE SYSTEM (10)",
            "TABLESAMPLE SYSTEM (10) WHERE",
        ),
        (
            "mysql",
            "SELECT * FROM orders o FORCE INDEX (idx_ts)",
            "FORCE INDEX (idx_ts) WHERE",
        ),
        (
            "tsql",
            "SELECT TOP 10 * FROM dbo.orders WITH (NOLOCK)",
            "WITH (NOLOCK) WHERE",
        ),
    ];
    for (dialect, sql, fragment) in cases {
        let out = filter(dialect, sql);
        assert!(out.contains(fragment), "{sql}\n=> {out}");
    }
}

#[test]
fn row_filter_lateral_and_correlated_references_still_bind() {
    let out = filter(
        "postgres",
        "SELECT o.id, x.sku FROM orders o \
         JOIN LATERAL (SELECT * FROM order_items i WHERE i.order_id = o.id ORDER BY i.price DESC LIMIT 3) x ON true",
    );
    assert_eq!(
        out,
        "SELECT o.id, x.sku FROM (SELECT * FROM orders WHERE tenant_id = 7) AS o \
         JOIN LATERAL (SELECT * FROM (SELECT * FROM order_items WHERE tenant_id = 7) AS i \
         WHERE i.order_id = o.id ORDER BY i.price DESC LIMIT 3) AS x ON TRUE"
    );
}

#[test]
fn row_filter_rejects_predicates_that_could_change_the_query_shape() {
    let opts = options("postgres");
    for predicate in [
        "tenant_id = 7; DROP TABLE orders",
        "tenant_id = 7 UNION SELECT 1",
        "1=1) OR (1=1",
        "SELECT 1",
        "   ",
    ] {
        let error = apply_row_filter("SELECT * FROM orders", predicate, &opts).unwrap_err();
        assert!(
            matches!(error.kind(), ErrorKind::InvalidArgument | ErrorKind::Parse),
            "{predicate:?}: {error}"
        );
    }
}

#[test]
fn row_filter_scopes_bigquery_tables_written_as_one_quoted_path() {
    // BigQuery users routinely quote the whole path in one pair of backticks.
    // `proj.ds.orders` is the same table as proj.ds.orders, so scoping by
    // name must not silently skip it: that would leak every tenant's rows.
    let opts = options("bigquery").table_names(["ds.orders"]);
    let unquoted = apply_row_filter("SELECT * FROM proj.ds.orders", TENANT, &opts).unwrap();
    assert_eq!(filter_count(&unquoted), 1, "{unquoted}");

    let quoted = apply_row_filter("SELECT * FROM `proj.ds.orders`", TENANT, &opts).unwrap();
    assert_eq!(filter_count(&quoted), 1, "table was not filtered: {quoted}");
}

/// How many times the tenant predicate was applied.
fn filter_count(sql: &str) -> usize {
    sql.matches("tenant_id = 7").count()
}

// ---------------------------------------------------------------------------
// rewrite_tables
// ---------------------------------------------------------------------------

#[test]
fn rewrite_tables_migrates_a_table_inside_ctes_and_subqueries() {
    let plan = [TableRewrite::inline(
        "sales.orders",
        TableRef::new("orders_v2").with_schema("curated"),
    )];
    let out = rewrite_tables(
        "WITH recent AS (SELECT * FROM sales.orders WHERE created_at > DATE '2024-01-01') \
         SELECT r.id FROM recent r WHERE r.id IN (SELECT o.id FROM sales.orders o WHERE o.status = 'PAID')",
        &plan,
        &options("trino"),
    )
    .unwrap();
    assert_eq!(table_counts(&out, "trino").unwrap().get("orders"), None, "{out}");
    assert_eq!(table_counts(&out, "trino").unwrap()["orders_v2"], 2, "{out}");
}

#[test]
fn rewrite_tables_unions_sharded_tables_and_rebinds_qualified_columns() {
    let plan = [TableRewrite::union(
        "sales.orders",
        UnionRewrite {
            table_alias: "orders".into(),
            columns: vec!["id".into(), "total".into()],
            branches: vec![
                TableRef::new("orders_2023").with_schema("archive"),
                TableRef::new("orders").with_schema("live"),
            ],
        },
    )];
    let out = rewrite_tables(
        "SELECT sales.orders.id, sum(sales.orders.total) FROM sales.orders GROUP BY sales.orders.id",
        &plan,
        &options("trino"),
    )
    .unwrap();
    assert_eq!(
        out,
        "SELECT orders.id, SUM(orders.total) FROM (SELECT id, total FROM archive.orders_2023 \
         UNION DISTINCT SELECT id, total FROM live.orders) AS orders GROUP BY orders.id"
    );
}

#[test]
fn rewrite_tables_matches_bigquery_paths_quoted_as_one_identifier() {
    let plan = [TableRewrite::inline(
        "ds.orders",
        TableRef::new("orders_v2").with_schema("ds"),
    )];
    let out = rewrite_tables("SELECT id FROM `ds.orders`", &plan, &options("bigquery")).unwrap();
    assert!(out.contains("orders_v2"), "`ds.orders` was not rewritten: {out}");
}

// ---------------------------------------------------------------------------
// inject_ctes
// ---------------------------------------------------------------------------

#[test]
fn inject_ctes_lets_existing_ctes_use_injected_ones() {
    // A semantic layer injects a tenant-scoped `visible_orders` that a
    // user-written CTE then builds on.
    let out = inject_ctes(
        "WITH daily AS (SELECT created_at, count(*) AS n FROM visible_orders GROUP BY created_at) SELECT * FROM daily",
        &[CteDef::new(
            "visible_orders",
            "SELECT * FROM orders WHERE tenant_id = 7",
        )],
        &options("postgres"),
    )
    .unwrap();
    assert_eq!(
        out,
        "WITH visible_orders AS (SELECT * FROM orders WHERE tenant_id = 7), \
         daily AS (SELECT created_at, COUNT(*) AS n FROM visible_orders GROUP BY created_at) SELECT * FROM daily"
    );
}

#[test]
fn inject_ctes_refuses_to_shadow_a_user_cte_regardless_of_case() {
    let error = inject_ctes(
        "WITH Visible_Orders AS (SELECT * FROM orders) SELECT * FROM visible_orders",
        &[CteDef::new(
            "visible_orders",
            "SELECT * FROM orders WHERE tenant_id = 7",
        )],
        &options("trino"),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
}

#[test]
fn inject_ctes_into_recursive_query_keeps_recursion() {
    let out = inject_ctes(
        "WITH RECURSIVE tree(id, parent_id) AS (SELECT id, parent_id FROM cats WHERE parent_id IS NULL \
         UNION ALL SELECT c.id, c.parent_id FROM cats c JOIN tree t ON c.parent_id = t.id) SELECT * FROM tree",
        &[CteDef::new("cats", "SELECT * FROM categories WHERE tenant_id = 7")],
        &options("postgres"),
    )
    .unwrap();
    assert!(out.starts_with("WITH RECURSIVE cats AS ("), "{out}");
    parse_one(&out, "postgres");
}

// ---------------------------------------------------------------------------
// column_origins
// ---------------------------------------------------------------------------

#[test]
fn origins_follow_aliases_through_ctes_and_derived_tables() {
    let opts = shop("postgres");
    let got = origins(
        "WITH paid AS (SELECT id AS order_id, total FROM orders WHERE status = 'PAID') \
         SELECT c.name, p.total FROM paid p JOIN customers c ON c.id = p.order_id",
        &opts,
    );
    assert_eq!(got, map(&[("orders", &["total"]), ("customers", &["name"])]));
}

#[test]
fn origins_of_a_grouped_cte_do_not_invent_columns_for_aggregate_aliases() {
    // The classic dbt model: aggregate in a CTE, then select from it by bare
    // name. `cnt` is computed by count(*); `orders` has no such column.
    let opts = shop("postgres");
    let sql = "WITH per_customer AS (SELECT customer_id, count(*) AS cnt FROM orders GROUP BY customer_id) \
               SELECT customer_id, cnt FROM per_customer";
    assert_eq!(origins(sql, &opts), map(&[("orders", &["customer_id"])]));

    let sql = "SELECT customer_id, cnt FROM \
               (SELECT customer_id, count(*) AS cnt FROM orders GROUP BY customer_id) per_customer";
    assert_eq!(origins(sql, &opts), map(&[("orders", &["customer_id"])]));
}

#[test]
fn origins_of_unqualified_column_from_a_joined_aggregate() {
    let opts = shop("postgres");
    let sql = "SELECT name, item_count FROM customers c \
               JOIN (SELECT o.customer_id, sum(i.qty) AS item_count \
                     FROM orders o JOIN order_items i ON i.order_id = o.id GROUP BY o.customer_id) s \
               ON s.customer_id = c.id";
    assert_eq!(
        origins(sql, &opts),
        map(&[("customers", &["name"]), ("orders", &[]), ("order_items", &["qty"])])
    );
}

#[test]
fn origins_star_except_drops_excluded_columns() {
    // PII-masking views commonly use SELECT * EXCEPT/EXCLUDE to drop email.
    let schema = [("customers", vec!["id", "name", "email"])];
    for (dialect, sql) in [
        ("bigquery", "SELECT * EXCEPT (email) FROM customers"),
        ("snowflake", "SELECT * EXCLUDE (email) FROM customers"),
        ("duckdb", "SELECT * EXCLUDE (email) FROM customers"),
    ] {
        let opts = options(dialect).schema(schema.clone());
        assert_eq!(
            origins(sql, &opts),
            map(&[("customers", &["id", "name"])]),
            "{dialect}: {sql}"
        );
    }
}

#[test]
fn origins_honour_schema_key_that_is_a_suffix_of_the_reference() {
    // Schemas are often keyed by bare table name while queries qualify it.
    let opts = options("postgres").schema([("orders", vec!["id", "total"])]);
    assert_eq!(
        origins("SELECT * FROM public.orders", &opts),
        map(&[("public.orders", &["id", "total"])])
    );
    let opts = options("trino").schema([("raw.orders", vec!["id", "total"])]);
    assert_eq!(
        origins("SELECT * FROM hive.raw.orders", &opts),
        map(&[("hive.raw.orders", &["id", "total"])])
    );
}

// ---------------------------------------------------------------------------
// output_columns
// ---------------------------------------------------------------------------

#[test]
fn output_columns_of_derived_table_match_the_equivalent_cte() {
    // Rewriting a CTE as an inline subquery must not lose the column names.
    let opts = options("postgres");
    let cte = outputs(
        "WITH s AS (SELECT id, total AS amount FROM orders) SELECT * FROM s",
        &opts,
    );
    assert_eq!(cte, ["id", "amount"]);
    let derived = outputs("SELECT * FROM (SELECT id, total AS amount FROM orders) s", &opts);
    assert_eq!(derived, cte);
}

#[test]
fn output_columns_use_derived_table_column_aliases() {
    let opts = options("postgres");
    assert_eq!(
        outputs(
            "SELECT * FROM (VALUES (1, 'new'), (2, 'paid')) AS v(code, label)",
            &opts
        ),
        ["code", "label"]
    );
    assert_eq!(
        outputs(
            "SELECT * FROM (SELECT id, total FROM orders) s(order_id, amount)",
            &opts
        ),
        ["order_id", "amount"]
    );
}

#[test]
fn output_columns_star_except_drops_excluded_columns() {
    let schema = [("customers", vec!["id", "name", "email"])];
    for (dialect, sql) in [
        ("bigquery", "SELECT * EXCEPT (email) FROM customers"),
        ("duckdb", "SELECT * EXCLUDE (email) FROM customers"),
    ] {
        let opts = options(dialect).schema(schema.clone());
        assert_eq!(outputs(sql, &opts), ["id", "name"], "{dialect}: {sql}");
    }
}

#[test]
fn output_columns_keep_case_of_schema_column_names() {
    // ORMs such as Prisma create camelCase columns in Postgres ("createdAt").
    // SELECT * returns them with their real names, not lower-cased.
    let opts = options("postgres").schema([("User", vec!["id", "createdAt", "displayName"])]);
    assert_eq!(
        outputs("SELECT * FROM \"User\"", &opts),
        ["id", "createdAt", "displayName"]
    );
}

#[test]
fn output_columns_honour_schema_key_that_is_a_suffix_of_the_reference() {
    let opts = options("postgres").schema([("orders", vec!["id", "total"])]);
    assert_eq!(outputs("SELECT * FROM public.orders", &opts), ["id", "total"]);
}

#[test]
fn output_columns_do_not_guess_between_ambiguous_schema_keys() {
    // Two schemas define `orders`; a bare reference cannot be resolved, so
    // the wildcard must stay unexpanded rather than pick one arbitrarily.
    let opts = options("postgres").schema([
        ("billing.orders", vec!["invoice_id"]),
        ("shop.orders", vec!["id", "total"]),
    ]);
    assert_eq!(outputs("SELECT * FROM orders", &opts), ["*"]);
}

// ---------------------------------------------------------------------------
// referenced_columns
// ---------------------------------------------------------------------------

#[test]
fn referenced_columns_include_filter_and_join_columns() {
    let got = referenced_columns(
        "SELECT c.name FROM customers c JOIN orders o ON o.customer_id = c.id \
         WHERE o.status = 'PAID' AND o.created_at >= DATE '2024-01-01'",
        &shop("postgres"),
    )
    .unwrap();
    assert_eq!(
        got,
        map(&[
            ("customers", &["id", "name"]),
            ("orders", &["created_at", "customer_id", "status"]),
        ])
    );
}

#[test]
fn referenced_columns_do_not_attribute_derived_columns_to_a_known_table() {
    // `orders` is in the schema and has no `item_count`; the column comes
    // from the derived table `s`.
    let got = referenced_columns(
        "SELECT item_count FROM orders \
         JOIN (SELECT order_id, count(*) AS item_count FROM order_items GROUP BY order_id) s \
         ON s.order_id = orders.id",
        &shop("postgres"),
    )
    .unwrap();
    assert_eq!(got, map(&[("orders", &["id"]), ("order_items", &["order_id"])]));
}

// ---------------------------------------------------------------------------
// Cases found while fixing the above
// ---------------------------------------------------------------------------

#[test]
fn star_over_join_using_outputs_the_shared_column_once() {
    let opts = shop("postgres");
    let sql = "SELECT * FROM orders JOIN customers USING (id)";
    assert_eq!(
        outputs(sql, &opts),
        ["id", "customer_id", "total", "status", "created_at", "name", "email"]
    );
    // The merged `id` may come from either table.
    assert_eq!(
        origins(sql, &opts),
        map(&[
            ("customers", &["email", "id", "name"]),
            ("orders", &["created_at", "customer_id", "id", "status", "total"]),
        ])
    );
    assert_eq!(
        outputs("SELECT * FROM orders NATURAL JOIN customers", &opts),
        ["id", "customer_id", "total", "status", "created_at", "name", "email"]
    );
    // A qualified star still outputs every column of its relation.
    assert_eq!(
        outputs("SELECT c.* FROM orders JOIN customers c USING (id)", &opts),
        ["id", "name", "email"]
    );
}

#[test]
fn star_over_a_table_function_is_unknown_not_empty() {
    let opts = shop("postgres");
    assert_eq!(outputs("SELECT * FROM generate_series(1, 3) g", &opts), ["*"]);
    assert_eq!(
        outputs("SELECT * FROM orders CROSS JOIN UNNEST(ARRAY[1, 2]) AS t(x)", &opts),
        ["*"]
    );
    assert_eq!(
        outputs("SELECT o.* FROM orders o CROSS JOIN UNNEST(ARRAY[1, 2]) AS t(x)", &opts),
        ["id", "customer_id", "total", "status", "created_at"]
    );
}

#[test]
fn star_over_union_by_name_combines_columns_by_name() {
    let opts = shop("duckdb");
    assert_eq!(
        outputs(
            "SELECT * FROM customers UNION ALL BY NAME SELECT * FROM order_items",
            &opts
        ),
        ["id", "name", "email", "order_id", "sku", "qty", "price"]
    );
}

#[test]
fn star_replace_and_rename_follow_the_modified_columns() {
    let opts = options("duckdb").schema([("orders", vec!["id", "total", "fx_rate"])]);
    let sql = "SELECT * REPLACE (total * fx_rate AS total) FROM orders";
    assert_eq!(outputs(sql, &opts), ["id", "total", "fx_rate"]);
    assert_eq!(origins(sql, &opts), map(&[("orders", &["fx_rate", "id", "total"])]));

    let opts = options("snowflake").schema([("orders", vec!["id", "customer_id", "total"])]);
    assert_eq!(
        outputs(
            "SELECT * EXCLUDE (total) RENAME (customer_id AS cid) FROM orders",
            &opts
        ),
        ["id", "cid"]
    );
}

#[test]
fn star_except_is_not_a_reference_to_the_excluded_column() {
    let opts = options("bigquery").schema([("customers", vec!["id", "name", "email"])]);
    let got = referenced_columns("SELECT * EXCEPT (email) FROM customers", &opts).unwrap();
    assert_eq!(got, map(&[("customers", &["id", "name"])]));
}

#[test]
fn star_qualified_by_a_schema_qualified_table_name() {
    let opts = options("trino").schema([("hive.raw.users", vec!["user_id", "user_name"])]);
    let sql = "SELECT hive.raw.users.* FROM hive.raw.users";
    assert_eq!(outputs(sql, &opts), ["user_id", "user_name"]);
    assert_eq!(
        origins(sql, &opts),
        map(&[("hive.raw.users", &["user_id", "user_name"])])
    );
}

#[test]
fn bigquery_quoted_path_qualifiers_are_rebound_to_the_filtered_table() {
    let opts = options("bigquery");
    let out = apply_row_filter(
        "SELECT `ds.orders`.id, `ds.orders`.* FROM `ds.orders` WHERE `ds.orders`.total > 0",
        TENANT,
        &opts,
    )
    .unwrap();
    assert_eq!(
        out,
        "SELECT `orders`.id, `orders`.* FROM (SELECT * FROM `ds`.`orders` WHERE tenant_id = 7) AS `orders` \
         WHERE `orders`.total > 0"
    );
    let got = referenced_columns("SELECT `proj.ds.orders`.id FROM `proj.ds.orders`", &opts).unwrap();
    assert_eq!(got, map(&[("proj.ds.orders", &["id"])]));
}
