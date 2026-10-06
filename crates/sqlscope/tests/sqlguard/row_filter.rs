//! Ported from sql-guard `easysql_test.go` (ApplyRowFilter).

use sqlscope::{apply_row_filter, ErrorKind, Options};

use crate::common::*;

const WHERE: &str = "tenant = 'alice'";
const MARKER: &str = "alice";

fn n() -> Options {
    Options::new()
}

/// Runs the filter and asserts that the output parses, has no unaliased
/// derived table and wraps exactly `wraps` tables (counted by `marker`).
fn rewrite_valid(dialect: &str, sql: &str, predicate: &str, marker: &str, wraps: usize, opts: Options) -> String {
    let out = apply_row_filter(sql, predicate, &opts.dialect(crate::common::dialect(dialect)))
        .unwrap_or_else(|e| panic!("apply_row_filter({sql:?}): {e}"));
    parse_one(&out, dialect);
    assert!(
        !has_unaliased_derived_table(&out, dialect),
        "empty derived-table alias:\n in: {sql}\nout: {out}"
    );
    assert_eq!(
        count_literals(&out, dialect, marker),
        wraps,
        "wrap count:\n in: {sql}\nout: {out}"
    );
    out
}

/// Applies the test predicate and asserts only that the output parses.
fn apply(dialect: &str, sql: &str, opts: Options) -> String {
    let out = apply_row_filter(sql, WHERE, &opts.dialect(crate::common::dialect(dialect)))
        .unwrap_or_else(|e| panic!("apply_row_filter({sql:?}): {e}"));
    parse_one(&out, dialect);
    out
}

fn wraps(out: &str, dialect: &str) -> usize {
    count_literals(out, dialect, MARKER)
}

#[test]
fn rewrite_structural() {
    let cases: Vec<(&str, &str, Options, &str, usize)> = vec![
        ("basic", WHERE, n(), "select * from a", 1),
        ("explicit alias", WHERE, n(), "select * from a t", 1),
        ("projection cols", WHERE, n(), "select id, name from a", 1),
        (
            "join wraps both",
            WHERE,
            n(),
            "select a.id from a join b on a.id = b.id",
            2,
        ),
        (
            "left join not degraded",
            WHERE,
            n(),
            "select * from a left join b on a.id = b.id",
            2,
        ),
        (
            "self join",
            WHERE,
            n(),
            "select t1.id from a t1 join a t2 on t1.id = t2.id",
            2,
        ),
        ("three way", WHERE, n(), "select * from a join b join c", 3),
        (
            "subquery in where",
            WHERE,
            n().table_names(["a"]),
            "select * from c where id in (select id from a)",
            1,
        ),
        ("derived table", WHERE, n(), "select * from (select * from a) x", 1),
        (
            "union both branches",
            WHERE,
            n(),
            "select * from a union select * from b",
            2,
        ),
        (
            "named scope only listed",
            WHERE,
            n().table_names(["a"]),
            "select * from a join c on a.id = c.id",
            1,
        ),
        (
            "out of scope untouched",
            WHERE,
            n().table_names(["a", "b"]),
            "select * from c",
            0,
        ),
        (
            "regex prefix",
            WHERE,
            n().table_patterns(["^log_"]),
            "select * from log_events join users on log_events.uid = users.id",
            1,
        ),
        (
            "names+regex compose",
            WHERE,
            n().table_names(["b"]).table_patterns(["^a$"]),
            "select * from a join b join c",
            2,
        ),
        (
            "disjunction where",
            "tenant = 'alice' or is_public = 1",
            n(),
            "select * from a",
            1,
        ),
        (
            "default db resolves",
            WHERE,
            n().table_names(["db.a"]).default_db("db"),
            "select * from a",
            1,
        ),
        (
            "index hint into subquery",
            WHERE,
            n(),
            "select * from a use index(idx)",
            1,
        ),
        (
            "partition into subquery",
            WHERE,
            n(),
            "select * from a partition(p0)",
            1,
        ),
    ];
    for (name, predicate, opts, sql, want) in cases {
        eprintln!("case: {name}");
        rewrite_valid("mysql", sql, predicate, MARKER, want, opts);
    }
}

#[test]
fn cte_references_not_wrapped() {
    for (sql, want) in [
        ("with c as (select * from a) select * from c", 1),
        ("with a as (select * from a) select * from a", 1),
        ("with a as (select 1 as id) select * from a", 0),
    ] {
        rewrite_valid("mysql", sql, WHERE, MARKER, want, n());
    }
}

#[test]
fn cte_scope_security() {
    let sql = "select * from t union all select * from (with t as (select 1 as id) select * from t) q";
    let out = rewrite_valid("mysql", sql, WHERE, MARKER, 1, n());
    assert!(
        out.contains("(SELECT * FROM t WHERE") || out.contains("(SELECT * FROM `t` WHERE"),
        "real table t was not filtered (security regression):\n{out}"
    );
}

#[test]
fn table_regexp_matches_qualified_name() {
    rewrite_valid(
        "mysql",
        "select * from sales.orders join sales.users on orders.user_id = users.id",
        WHERE,
        MARKER,
        1,
        n().table_patterns([r"^sales\.orders$"]),
    );
}

#[test]
fn table_functions_not_wrapped() {
    for (dialect, sql) in [
        ("postgres", "select * from generate_series(1, 10)"),
        ("starrocks", "select * from table(generator(10))"),
        ("trino", "select * from unnest(array[1,2]) with ordinality"),
    ] {
        rewrite_valid(dialect, sql, WHERE, MARKER, 0, n());
    }
    rewrite_valid(
        "trino",
        "select x from a cross join unnest(a.arr) as t(x)",
        WHERE,
        MARKER,
        1,
        n(),
    );
}

#[test]
fn quoting_preserved() {
    let out = rewrite_valid("postgres", r#"select * from "order""#, WHERE, MARKER, 1, n());
    assert!(
        out.contains(r#""order""#),
        "quoting on reserved-word identifier was lost: {out}"
    );
}

#[test]
fn dual_skipped() {
    rewrite_valid("mysql", "select 1 from dual", WHERE, MARKER, 0, n());
}

#[test]
fn where_expression_preserved() {
    let out = rewrite_valid(
        "mysql",
        "select * from a join b on a.id = b.id",
        "tenant = 'acme'",
        "acme",
        2,
        n(),
    );
    assert_eq!(
        out.matches("'acme'").count(),
        2,
        "where expression not applied to each table: {out}"
    );
}

#[test]
fn schema_strip() {
    let out = rewrite_valid(
        "mysql",
        "select sales.orders.id, name from sales.orders",
        WHERE,
        MARKER,
        1,
        n(),
    );
    assert!(
        !out.contains("sales.orders.id"),
        "three-part column reference was not stripped to the alias:\n{out}"
    );
    assert!(out.contains("orders.id"), "expected stripped column orders.id:\n{out}");
}

#[test]
fn errors() {
    // sql-guard defaulted to MySQL; sqlscope defaults to Trino, so the dialect
    // is set explicitly.
    for (name, sql, want) in [
        ("update", "update a set x = 1", ErrorKind::Unsupported),
        ("delete", "delete from a", ErrorKind::Unsupported),
        ("insert", "insert into a values (1)", ErrorKind::Unsupported),
        (
            "multiple statements",
            "select * from a; select * from b",
            ErrorKind::Unsupported,
        ),
        ("syntax error", "select * frm where", ErrorKind::Parse),
    ] {
        let error = apply_row_filter(sql, WHERE, &options("mysql")).unwrap_err();
        assert_eq!(error.kind(), want, "{name}: {error}");
    }
}

#[test]
fn config_validation() {
    // sql-guard accepted four dialects and rejected "oracle"; sqlscope
    // supports Oracle, so an unknown name stands in for it.
    assert!("nosuchdialect".parse::<sqlscope::Dialect>().is_err());
    for (name, predicate, opts) in [
        ("empty where", "", n()),
        ("invalid where", "user ===", n()),
        ("empty regexp", WHERE, n().table_patterns([""])),
        ("invalid regexp", WHERE, n().table_patterns(["("])),
    ] {
        assert!(
            apply_row_filter("select * from a", predicate, &opts).is_err(),
            "{name}: where={predicate:?} succeeded, want error"
        );
    }
}

/// `WithSelfCheck` was a no-op in sql-guard and has no sqlscope counterpart;
/// the wrap counts it pinned still hold.
#[test]
fn self_check() {
    for (sql, want) in [
        ("select * from a", 1),
        ("select a.id from a join b on a.id = b.id", 2),
        ("select * from a union select * from b", 2),
        ("with c as (select * from a) select * from c", 1),
    ] {
        let first = rewrite_valid("mysql", sql, WHERE, MARKER, want, n());
        let second = apply_row_filter(sql, WHERE, &options("mysql")).unwrap();
        assert_eq!(first, second, "output is not deterministic for {sql}");
    }
}

#[test]
fn dialect_boundary() {
    let cases = [
        ("postgres", "cast ::", "select id::text from a", 1),
        ("postgres", "ILIKE", "select * from a where name ilike 'a%'", 1),
        (
            "postgres",
            "distinct on",
            "select distinct on (uid) uid, ts from a order by uid, ts desc",
            1,
        ),
        ("postgres", "array literal", "select array[1,2,3] from a", 1),
        ("postgres", "array index", "select tags[1] from a", 1),
        ("postgres", "regex ~", "select * from a where name ~ '^x'", 1),
        ("postgres", "concat ||", "select first || last from a", 1),
        ("postgres", "is true", "select * from a where flag is true", 1),
        (
            "postgres",
            "fetch first",
            "select * from a order by id fetch first 10 rows only",
            1,
        ),
        ("postgres", "limit offset", "select * from a limit 10 offset 5", 1),
        ("postgres", "dollar param", "select * from a where id = $1", 1),
        ("postgres", "dquoted ident", r#"select "uid" from a"#, 1),
        (
            "postgres",
            "lateral",
            "select * from a, lateral (select * from b where b.aid = a.id) s",
            2,
        ),
        ("postgres", "generate_series", "select * from generate_series(1, 10)", 0),
        (
            "postgres",
            "filter clause",
            "select count(*) filter (where x > 0) from a",
            1,
        ),
        ("postgres", "json arrow", "select data->>'k' from a", 1),
        (
            "postgres",
            "interval",
            "select * from a where ts > now() - interval '1 day'",
            1,
        ),
        ("trino", "unnest", "select x from a cross join unnest(a.arr) as t(x)", 1),
        ("trino", "try_cast", "select try_cast(x as bigint) from a", 1),
        ("trino", "row ctor", "select row(1, 'a') from a", 1),
        ("trino", "map subscript", "select m['k'] from a", 1),
        ("trino", "lambda", "select filter(arr, x -> x > 0) from a", 1),
        (
            "trino",
            "grouping sets",
            "select k, sum(v) from a group by grouping sets ((k), ())",
            1,
        ),
        ("trino", "cube", "select k, sum(v) from a group by cube (k)", 1),
        ("trino", "rollup", "select k, sum(v) from a group by rollup (k)", 1),
        ("trino", "concat ||", "select a || b from a", 1),
        ("trino", "dquoted ident", r#"select "count" from a"#, 1),
        ("trino", "catalog.schema.table", "select * from hive.sales.a", 1),
        ("trino", "tablesample", "select * from a tablesample bernoulli (10)", 1),
        (
            "trino",
            "with ordinality",
            "select * from unnest(array[1,2]) with ordinality",
            0,
        ),
        ("trino", "decimal cast", "select cast(x as decimal(10,2)) from a", 1),
        ("starrocks", "array type cast", "select cast(x as array<int>) from a", 1),
        ("starrocks", "named struct", "select named_struct('a', 1) from a", 1),
        (
            "starrocks",
            "broadcast hint",
            "select * from a join [broadcast] b on a.id = b.id",
            2,
        ),
        (
            "starrocks",
            "bucket_shuffle hint",
            "select * from a join [bucket_shuffle] b on a.id = b.id",
            2,
        ),
        ("starrocks", "table function", "select * from table(generator(10))", 0),
        (
            "starrocks",
            "array map lambda",
            "select array_map(x -> x + 1, arr) from a",
            1,
        ),
        (
            "starrocks",
            "qualify",
            "select id, row_number() over (partition by k order by ts) rn from a qualify rn = 1",
            1,
        ),
        (
            "starrocks",
            "set_var hint",
            "select /*+ SET_VAR(query_timeout=5) */ * from a",
            1,
        ),
        ("mysql", "plain", "select * from a", 1),
        (
            "mysql",
            "window",
            "select row_number() over (partition by k order by ts) from a",
            1,
        ),
        ("mysql", "cte", "with c as (select * from a) select * from c", 1),
        (
            "mysql",
            "case when",
            "select case when x > 0 then 1 else 0 end from a",
            1,
        ),
        ("mysql", "union all", "select * from a union all select * from b", 2),
    ];
    for (dialect, name, sql, want) in cases {
        eprintln!("case: {dialect}/{name}");
        rewrite_valid(dialect, sql, WHERE, MARKER, want, n());
    }
}

#[test]
fn bughunt_duplicate_alias_same_bare_name_join() {
    let sql = "select s1.t.id, s2.t.name from s1.t join s2.t on s1.t.id = s2.t.id";
    let out = apply("mysql", sql, n());
    assert_eq!(
        duplicate_in_scope(&out, "mysql"),
        None,
        "duplicate relation name in one FROM:\n in: {sql}\nout: {out}"
    );
}

#[test]
fn apply_row_filter_bare_rebind_is_scoped_to_each_query() {
    let out = apply_row_filter(
        "SELECT t.a FROM s1.t WHERE t.b IN (SELECT t.c FROM s2.t)",
        "x = 1",
        &options("trino"),
    )
    .unwrap();
    assert_eq!(
        out,
        "SELECT s1_t.a FROM (SELECT * FROM s1.t WHERE x = 1) AS s1_t WHERE s1_t.b IN (SELECT s2_t.c FROM (SELECT * FROM s2.t WHERE x = 1) AS s2_t)"
    );
}

#[test]
fn bughunt_cte_name_collides_with_derived_alias() {
    let sql = "with t as (select 1 as id) select db.t.id, t.id from db.t join t on db.t.id = t.id";
    let out = apply("mysql", sql, n());
    assert_eq!(
        duplicate_in_scope(&out, "mysql"),
        None,
        "duplicate relation name in one FROM:\n in: {sql}\nout: {out}"
    );
}

#[test]
fn bughunt_recursive_cte_self_reference() {
    let sql = "with recursive c as (select 1 as n union all select n + 1 from c where n < 3) select * from c";
    let out = apply("mysql", sql, n());
    assert_eq!(wraps(&out, "mysql"), 0, "recursive CTE self-reference wrapped: {out}");
}

#[test]
fn bughunt_qualified_star_not_rebound() {
    let sql = "select sales.orders.*, 1 from sales.orders";
    let out = apply("mysql", sql, n());
    assert!(
        out.contains("orders.*"),
        "expected qualified star rebound to orders.*:\n in: {sql}\nout: {out}"
    );
}

#[test]
fn bughunt_catalog_qualified_column_ref_not_stripped() {
    let sql = "select hive.sales.a.col1 from hive.sales.a";
    let out = apply("trino", sql, n());
    assert!(
        out.contains("a.col1"),
        "expected column ref rebound to a.col1:\n in: {sql}\nout: {out}"
    );
}

#[test]
fn bughunt_subquery_in_exists() {
    let out = apply(
        "mysql",
        "select * from c where exists (select 1 from a where a.cid = c.id)",
        n().table_names(["a"]),
    );
    assert_eq!(wraps(&out, "mysql"), 1, "{out}");
}

#[test]
fn bughunt_set_ops_and_cte_shadowing() {
    let sql =
        "select * from t union select * from (with t as (select 1 as id) select * from t) q intersect select * from t";
    let out = apply("mysql", sql, n());
    assert_eq!(wraps(&out, "mysql"), 2, "{out}");
}

#[test]
fn bughunt_self_join_aliases() {
    let out = apply(
        "mysql",
        "select t1.id from sales.orders t1 join sales.orders t2 on t1.pid = t2.id",
        n(),
    );
    assert_eq!(wraps(&out, "mysql"), 2, "{out}");
    let names = relation_names(&out, "mysql");
    for alias in ["t1", "t2"] {
        assert!(
            names.iter().any(|name| name == alias),
            "explicit alias {alias} lost: {out}"
        );
    }
}

#[test]
fn bughunt_lateral() {
    let out = apply(
        "postgres",
        "select * from a, lateral (select * from b where b.aid = a.id) s",
        n(),
    );
    assert_eq!(wraps(&out, "postgres"), 2, "{out}");
}

#[test]
fn bughunt_dialect_quoting() {
    let pg = apply("postgres", r#"select * from "order""#, n());
    assert!(pg.contains(r#""order""#), "postgres quoting lost: {pg}");
    let my = apply("mysql", "select * from `order`", n());
    assert!(my.contains("`order`"), "mysql backtick quoting lost: {my}");
}

#[test]
fn bughunt_table_names_matching() {
    let out = apply("mysql", "select * from sales.orders", n().table_names(["orders"]));
    assert_eq!(wraps(&out, "mysql"), 1, "bare scope should match sales.orders: {out}");
    let out = apply("mysql", "select * from orders", n().table_names(["sales.orders"]));
    assert_eq!(wraps(&out, "mysql"), 0, "qualified scope matched a bare ref: {out}");
    let out = apply(
        "mysql",
        "select * from orders",
        n().table_names(["sales.orders"]).default_db("sales"),
    );
    assert_eq!(wraps(&out, "mysql"), 1, "default db should resolve the bare ref: {out}");
    let out = apply(
        "mysql",
        "select * from hr.orders",
        n().table_names(["sales.orders"]).default_db("sales"),
    );
    assert_eq!(wraps(&out, "mysql"), 0, "hr.orders must not match sales.orders: {out}");
}

#[test]
fn apply_row_filter_catalog_qualified_table_names() {
    let table = "rda_launch_to_engage.interaction";
    let scope = || n().table_names(["iceberg.rda_launch_to_engage.interaction"]);
    let out = apply("trino", &format!("select * from iceberg.{table}"), scope());
    assert_eq!(wraps(&out, "trino"), 1, "{out}");
    let out = apply("trino", &format!("select * from hive.{table}"), scope());
    assert_eq!(wraps(&out, "trino"), 0, "scope leaked across catalogs: {out}");
    let out = apply(
        "trino",
        &format!("select * from iceberg.{table}"),
        n().table_names([table]),
    );
    assert_eq!(
        wraps(&out, "trino"),
        1,
        "schema-qualified scope should match through a catalog: {out}"
    );
}

#[test]
fn bughunt_cte_sibling_scope() {
    let sql = "select * from (with x as (select 1 as id) select * from x) a join x on a.id = x.id";
    let out = apply("mysql", sql, n());
    assert_eq!(wraps(&out, "mysql"), 1, "{out}");
}

#[test]
fn bughunt_where_clause_injection_rejected() {
    for predicate in ["1=1) as x --", "1=1 union select * from secrets", "1=1; drop table a"] {
        assert!(
            apply_row_filter("select * from a", predicate, &options("mysql")).is_err(),
            "predicate {predicate:?} accepted"
        );
    }
    let out = apply_row_filter("select * from a", "tenant = 'alice' -- boom", &options("mysql")).unwrap();
    assert_eq!(count_literals(&out, "mysql", "alice"), 1, "{out}");
}

#[test]
fn bughunt_double_apply_still_valid() {
    let once = apply("mysql", "select * from a", n());
    let twice = apply("mysql", &once, n());
    assert!(
        wraps(&twice, "mysql") >= 1,
        "second application lost the filter: {twice}"
    );
}

#[test]
fn bughunt_quoted_case_insensitive_scope() {
    let out = apply("postgres", r#"select * from "Orders""#, n().table_names(["orders"]));
    assert!(out.contains(r#""Orders""#), "quoted identifier casing lost: {out}");
}

#[test]
fn bughunt_table_functions() {
    let out = apply("postgres", "select * from generate_series(1, 10) g join a on true", n());
    assert_eq!(wraps(&out, "postgres"), 1, "{out}");
}
