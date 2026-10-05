#![allow(clippy::type_complexity)]

mod common;

use common::*;
use sqlscope::{apply_row_filter, ErrorKind, Options};

const WHERE: &str = "tenant = 'alice'";
const MARKER: &str = "alice";

/// Applies the filter and asserts the output parses, has no unaliased derived
/// table and wraps exactly `wraps` tables.
fn rewrite_valid(dialect: &str, sql: &str, predicate: &str, marker: &str, wraps: usize, opts: Options) -> String {
    let out = apply_row_filter(sql, predicate, &opts.dialect(common::dialect(dialect)))
        .unwrap_or_else(|e| panic!("apply_row_filter({sql:?}): {e}"));
    assert!(
        !has_unaliased_derived_table(&out, dialect),
        "unaliased derived table:\n in: {sql}\nout: {out}"
    );
    assert_eq!(
        count_literals(&out, dialect, marker),
        wraps,
        "wrap count:\n in: {sql}\nout: {out}"
    );
    out
}

fn wraps(dialect: &str, sql: &str, opts: Options) -> usize {
    let out = apply_row_filter(sql, WHERE, &opts.dialect(common::dialect(dialect)))
        .unwrap_or_else(|e| panic!("apply_row_filter({sql:?}): {e}"));
    count_literals(&out, dialect, MARKER)
}

fn apply(dialect: &str, sql: &str) -> String {
    let out = apply_row_filter(sql, WHERE, &options(dialect)).unwrap_or_else(|e| panic!("{sql}: {e}"));
    parse_one(&out, dialect);
    out
}

#[test]
fn structural() {
    let n = Options::new;
    let cases: Vec<(&str, Options, &str, usize)> = vec![
        (WHERE, n(), "select * from a", 1),
        (WHERE, n(), "select * from a t", 1),
        (WHERE, n(), "select id, name from a", 1),
        (WHERE, n(), "select a.id from a join b on a.id = b.id", 2),
        (WHERE, n(), "select * from a left join b on a.id = b.id", 2),
        (WHERE, n(), "select t1.id from a t1 join a t2 on t1.id = t2.id", 2),
        (WHERE, n(), "select * from a join b join c", 3),
        (
            WHERE,
            n().table_names(["a"]),
            "select * from c where id in (select id from a)",
            1,
        ),
        (WHERE, n(), "select * from (select * from a) x", 1),
        (WHERE, n(), "select * from a union select * from b", 2),
        (
            WHERE,
            n().table_names(["a"]),
            "select * from a join c on a.id = c.id",
            1,
        ),
        (WHERE, n().table_names(["a", "b"]), "select * from c", 0),
        (
            WHERE,
            n().table_patterns(["^log_"]),
            "select * from log_events join users on log_events.uid = users.id",
            1,
        ),
        (
            WHERE,
            n().table_names(["b"]).table_patterns(["^a$"]),
            "select * from a join b join c",
            2,
        ),
        ("tenant = 'alice' or is_public = 1", n(), "select * from a", 1),
        (WHERE, n().table_names(["db.a"]).default_db("db"), "select * from a", 1),
        (WHERE, n(), "select * from a use index(idx)", 1),
        (WHERE, n(), "select * from a partition(p0)", 1),
    ];
    for (predicate, opts, sql, expected) in cases {
        rewrite_valid("mysql", sql, predicate, MARKER, expected, opts);
    }
}

#[test]
fn basic_output_is_exact() {
    let out = apply_row_filter("SELECT id FROM orders", "tenant_id = 7", &options("postgres")).unwrap();
    assert_eq!(
        out,
        "SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders"
    );
}

#[test]
fn cte_references_are_not_wrapped() {
    for (sql, expected) in [
        ("with c as (select * from a) select * from c", 1),
        ("with a as (select * from a) select * from a", 1),
        ("with a as (select 1 as id) select * from a", 0),
    ] {
        rewrite_valid("mysql", sql, WHERE, MARKER, expected, Options::new());
    }
}

#[test]
fn cte_in_one_branch_does_not_hide_real_table_in_sibling() {
    let sql = "select * from t union all select * from (with t as (select 1 as id) select * from t) q";
    let out = rewrite_valid("mysql", sql, WHERE, MARKER, 1, Options::new());
    assert!(
        out.contains("(SELECT * FROM t WHERE") || out.contains("(SELECT * FROM `t` WHERE"),
        "{out}"
    );
}

#[test]
fn table_pattern_matches_qualified_name() {
    rewrite_valid(
        "mysql",
        "select * from sales.orders join sales.users on orders.user_id = users.id",
        WHERE,
        MARKER,
        1,
        Options::new().table_patterns([r"^sales\.orders$"]),
    );
}

#[test]
fn table_functions_are_not_wrapped() {
    for (dialect, sql) in [
        ("postgres", "select * from generate_series(1, 10)"),
        ("starrocks", "select * from table(generator(10))"),
        ("trino", "select * from unnest(array[1,2]) with ordinality"),
    ] {
        rewrite_valid(dialect, sql, WHERE, MARKER, 0, Options::new());
    }
    rewrite_valid(
        "trino",
        "select x from a cross join unnest(a.arr) as t(x)",
        WHERE,
        MARKER,
        1,
        Options::new(),
    );
    assert_eq!(
        wraps(
            "postgres",
            "select * from generate_series(1, 10) g join a on true",
            Options::new()
        ),
        1
    );
}

#[test]
fn quoting_is_preserved() {
    assert!(apply("postgres", r#"select * from "order""#).contains(r#""order""#));
    assert!(apply("mysql", "select * from `order`").contains("`order`"));
    let out = apply_row_filter(
        r#"select * from "Orders""#,
        WHERE,
        &options("postgres").table_names(["orders"]),
    )
    .unwrap();
    assert!(out.contains(r#""Orders""#), "{out}");
}

#[test]
fn dual_is_skipped() {
    rewrite_valid("mysql", "select 1 from dual", WHERE, MARKER, 0, Options::new());
}

#[test]
fn predicate_is_applied_per_table() {
    let out = rewrite_valid(
        "mysql",
        "select * from a join b on a.id = b.id",
        "tenant = 'acme'",
        "acme",
        2,
        Options::new(),
    );
    assert_eq!(out.matches("'acme'").count(), 2, "{out}");
}

#[test]
fn schema_qualified_column_is_rebound_to_alias() {
    let out = rewrite_valid(
        "mysql",
        "select sales.orders.id, name from sales.orders",
        WHERE,
        MARKER,
        1,
        Options::new(),
    );
    assert!(!out.contains("sales.orders.id"), "{out}");
    assert!(out.contains("orders.id"), "{out}");
}

#[test]
fn errors_are_classified() {
    for (sql, kind) in [
        ("update a set x = 1", ErrorKind::Unsupported),
        ("delete from a", ErrorKind::Unsupported),
        ("insert into a values (1)", ErrorKind::Unsupported),
        ("select * from a; select * from b", ErrorKind::Unsupported),
        ("select * frm where", ErrorKind::Parse),
    ] {
        let err = apply_row_filter(sql, WHERE, &Options::new()).unwrap_err();
        assert_eq!(err.kind(), kind, "{sql}: {err}");
    }
}

#[test]
fn invalid_configuration_is_rejected() {
    for (predicate, opts) in [
        ("", Options::new()),
        ("user ===", Options::new()),
        (WHERE, Options::new().table_patterns([""])),
        (WHERE, Options::new().table_patterns(["("])),
        (WHERE, Options::new().table_names(Vec::<String>::new())),
        (WHERE, Options::new().table_names(["a..b"])),
        (WHERE, Options::new().table_names(["a.b.c.d"])),
    ] {
        assert!(
            apply_row_filter("select * from a", predicate, &opts).is_err(),
            "{predicate:?} accepted"
        );
    }
    assert!("not-a-dialect".parse::<sqlscope::Dialect>().is_err());
}

#[test]
fn predicate_injection_is_rejected() {
    for predicate in [
        "1=1) as x --",
        "1=1 union select * from secrets",
        "1=1; drop table a",
        "1=1 order by 1",
        "1=1 limit 1",
        "1=1 group by x",
    ] {
        assert!(
            apply_row_filter("select * from a", predicate, &options("mysql")).is_err(),
            "{predicate:?} accepted"
        );
    }
    let out = apply_row_filter("select * from a", "tenant = 'alice' -- boom", &options("mysql")).unwrap();
    assert_eq!(count_literals(&out, "mysql", "alice"), 1, "{out}");
}

#[test]
fn dialect_coverage() {
    let cases = [
        ("postgres", "select id::text from a", 1),
        ("postgres", "select * from a where name ilike 'a%'", 1),
        (
            "postgres",
            "select distinct on (uid) uid, ts from a order by uid, ts desc",
            1,
        ),
        ("postgres", "select array[1,2,3] from a", 1),
        ("postgres", "select tags[1] from a", 1),
        ("postgres", "select * from a where name ~ '^x'", 1),
        ("postgres", "select first || last from a", 1),
        ("postgres", "select * from a where flag is true", 1),
        ("postgres", "select * from a order by id fetch first 10 rows only", 1),
        ("postgres", "select * from a limit 10 offset 5", 1),
        ("postgres", "select * from a where id = $1", 1),
        ("postgres", r#"select "uid" from a"#, 1),
        (
            "postgres",
            "select * from a, lateral (select * from b where b.aid = a.id) s",
            2,
        ),
        ("postgres", "select * from generate_series(1, 10)", 0),
        ("postgres", "select count(*) filter (where x > 0) from a", 1),
        ("postgres", "select data->>'k' from a", 1),
        ("postgres", "select * from a where ts > now() - interval '1 day'", 1),
        ("trino", "select x from a cross join unnest(a.arr) as t(x)", 1),
        ("trino", "select try_cast(x as bigint) from a", 1),
        ("trino", "select row(1, 'a') from a", 1),
        ("trino", "select m['k'] from a", 1),
        ("trino", "select filter(arr, x -> x > 0) from a", 1),
        ("trino", "select k, sum(v) from a group by grouping sets ((k), ())", 1),
        ("trino", "select k, sum(v) from a group by cube (k)", 1),
        ("trino", "select k, sum(v) from a group by rollup (k)", 1),
        ("trino", "select a || b from a", 1),
        ("trino", r#"select "count" from a"#, 1),
        ("trino", "select * from hive.sales.a", 1),
        ("trino", "select * from a tablesample bernoulli (10)", 1),
        ("trino", "select * from unnest(array[1,2]) with ordinality", 0),
        ("trino", "select cast(x as decimal(10,2)) from a", 1),
        ("starrocks", "select cast(x as array<int>) from a", 1),
        ("starrocks", "select named_struct('a', 1) from a", 1),
        ("starrocks", "select * from a join [broadcast] b on a.id = b.id", 2),
        ("starrocks", "select * from a join [bucket_shuffle] b on a.id = b.id", 2),
        ("starrocks", "select * from table(generator(10))", 0),
        ("starrocks", "select array_map(x -> x + 1, arr) from a", 1),
        (
            "starrocks",
            "select id, row_number() over (partition by k order by ts) rn from a qualify rn = 1",
            1,
        ),
        ("starrocks", "select /*+ SET_VAR(query_timeout=5) */ * from a", 1),
        ("mysql", "select * from a", 1),
        (
            "mysql",
            "select row_number() over (partition by k order by ts) from a",
            1,
        ),
        ("mysql", "with c as (select * from a) select * from c", 1),
        ("mysql", "select case when x > 0 then 1 else 0 end from a", 1),
        ("mysql", "select * from a union all select * from b", 2),
    ];
    let mut failures = Vec::new();
    for (dialect, sql, expected) in cases {
        let result = std::panic::catch_unwind(|| rewrite_valid(dialect, sql, WHERE, MARKER, expected, Options::new()));
        if result.is_err() {
            failures.push(format!("{dialect}: {sql}"));
        }
    }
    assert!(
        failures.is_empty(),
        "dialect coverage failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn same_bare_name_in_two_schemas_gets_distinct_aliases() {
    let sql = "select s1.t.id, s2.t.name from s1.t join s2.t on s1.t.id = s2.t.id";
    let out = apply("mysql", sql);
    assert_eq!(duplicate_in_scope(&out, "mysql"), None, "{out}");
}

#[test]
fn bare_rebinding_is_scoped_to_each_query() {
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
fn derived_alias_avoids_same_named_cte() {
    let out = apply(
        "mysql",
        "with t as (select 1 as id) select db.t.id, t.id from db.t join t on db.t.id = t.id",
    );
    assert_eq!(duplicate_in_scope(&out, "mysql"), None, "{out}");
}

#[test]
fn recursive_cte_self_reference_is_not_wrapped() {
    let out = apply(
        "mysql",
        "with recursive c as (select 1 as n union all select n + 1 from c where n < 3) select * from c",
    );
    assert_eq!(count_literals(&out, "mysql", MARKER), 0, "{out}");
}

#[test]
fn qualified_star_and_catalog_columns_are_rebound() {
    assert!(apply("mysql", "select sales.orders.*, 1 from sales.orders").contains("orders.*"));
    assert!(apply("trino", "select hive.sales.a.col1 from hive.sales.a").contains("a.col1"));
}

#[test]
fn subqueries_set_operations_and_lateral() {
    assert_eq!(
        wraps(
            "mysql",
            "select * from c where exists (select 1 from a where a.cid = c.id)",
            Options::new().table_names(["a"])
        ),
        1
    );
    assert_eq!(
        wraps(
            "mysql",
            "select * from t union select * from (with t as (select 1 as id) select * from t) q intersect select * from t",
            Options::new()
        ),
        2
    );
    assert_eq!(
        wraps(
            "postgres",
            "select * from a, lateral (select * from b where b.aid = a.id) s",
            Options::new()
        ),
        2
    );
    assert_eq!(
        wraps(
            "mysql",
            "select * from (with x as (select 1 as id) select * from x) a join x on a.id = x.id",
            Options::new()
        ),
        1
    );
}

#[test]
fn self_join_keeps_explicit_aliases() {
    let out = apply(
        "mysql",
        "select t1.id from sales.orders t1 join sales.orders t2 on t1.pid = t2.id",
    );
    assert_eq!(count_literals(&out, "mysql", MARKER), 2);
    let names = relation_names(&out, "mysql");
    assert!(
        names.contains(&"t1".to_string()) && names.contains(&"t2".to_string()),
        "{out}"
    );
}

#[test]
fn table_name_matching() {
    assert_eq!(
        wraps(
            "mysql",
            "select * from sales.orders",
            Options::new().table_names(["orders"])
        ),
        1
    );
    assert_eq!(
        wraps(
            "mysql",
            "select * from orders",
            Options::new().table_names(["sales.orders"])
        ),
        0
    );
    assert_eq!(
        wraps(
            "mysql",
            "select * from orders",
            Options::new().table_names(["sales.orders"]).default_db("sales")
        ),
        1
    );
    assert_eq!(
        wraps(
            "mysql",
            "select * from hr.orders",
            Options::new().table_names(["sales.orders"]).default_db("sales")
        ),
        0
    );
}

#[test]
fn catalog_qualified_scope() {
    let table = "rda_launch_to_engage.interaction";
    let full = format!("iceberg.{table}");
    assert_eq!(
        wraps(
            "trino",
            &format!("select * from iceberg.{table}"),
            Options::new().table_names([full.clone()])
        ),
        1
    );
    assert_eq!(
        wraps(
            "trino",
            &format!("select * from hive.{table}"),
            Options::new().table_names([full.clone()])
        ),
        0
    );
    assert_eq!(
        wraps(
            "trino",
            &format!("select * from iceberg.{table}"),
            Options::new().table_names([table])
        ),
        1
    );
    let pattern = r"^iceberg\.rda_launch_to_engage\.interaction$";
    assert_eq!(
        wraps(
            "trino",
            &format!("select * from iceberg.{table}"),
            Options::new().table_patterns([pattern])
        ),
        1
    );
    assert_eq!(
        wraps(
            "trino",
            &format!("select * from hive.{table}"),
            Options::new().table_patterns([pattern])
        ),
        0
    );
}

#[test]
fn applying_twice_stays_valid() {
    let once = apply("mysql", "select * from a");
    let twice = apply("mysql", &once);
    assert!(count_literals(&twice, "mysql", MARKER) >= 1);
}

#[test]
fn input_guard() {
    let deep_func = format!("select s{}1{} from a", "(".repeat(4000), ")".repeat(4000));
    let deep_unary = format!("select {}1 from a", "~ ".repeat(4000));
    let huge = format!("select * from a where x in ({}1)", "1,".repeat(1 << 20));
    for sql in [deep_func, deep_unary, huge] {
        let err = apply_row_filter(&sql, "t = 1", &options("mysql")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported, "{err}");
    }
    let deep_predicate = format!("{}1{}", "(".repeat(4000), ")".repeat(4000));
    let err = apply_row_filter("SELECT * FROM t", &deep_predicate, &Options::new()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported, "{err}");

    for sql in [
        format!("select '{}' from a", "(".repeat(300)),
        format!("select /* {} */ id from a", "{".repeat(300)),
        "select * from a where note = '{a:{b:{c}}}'".to_string(),
        "select * from a where x in (1, (2), ((3)))".to_string(),
    ] {
        apply_row_filter(&sql, "t = 1", &options("mysql")).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// Oracle checks independent of the rewrite logic: a no-op scope returns
/// the input byte-for-byte; a full rewrite parses and preserves the multiset
/// of physical table names.
#[test]
fn invariants() {
    let corpus = [
        ("mysql", "select * from a"),
        ("mysql", "select id, Name from Orders o where o.x = 1"),
        ("mysql", "select a.id from a join b on a.id = b.id"),
        ("mysql", "select * from a left join b on a.id = b.id"),
        ("mysql", "select t1.id from a t1 join a t2 on t1.id = t2.id"),
        ("mysql", "select * from a join b join c"),
        ("mysql", "select * from a, b, c"),
        ("mysql", "select * from c where id in (select id from a)"),
        ("mysql", "select * from (select * from a) x"),
        ("mysql", "select * from a union select * from b"),
        (
            "mysql",
            "select * from a union all select * from b intersect select * from c",
        ),
        ("mysql", "select * from a use index(idx)"),
        ("mysql", "select * from a partition(p0)"),
        ("mysql", "select 1 from dual"),
        ("mysql", "with c as (select * from a) select * from c"),
        ("mysql", "with a as (select * from a) select * from a"),
        (
            "mysql",
            "select * from t union all select * from (with t as (select 1 as id) select * from t) q",
        ),
        ("mysql", "select (select count(*) from x) n, a.id from a"),
        ("mysql", "/* lead */ select a.id from a -- tail\nwhere a.x = 1"),
        ("mysql", "select row_number() over (partition by k order by ts) from a"),
        ("mysql", "select case when x > 0 then 1 else 0 end from a"),
        ("mysql", "select 姓名 from 订单 where city = '北京' -- 备注"),
        ("mysql", "select * from a;"),
        ("postgres", "select * from sales.orders"),
        ("postgres", r#"select * from "order""#),
        ("postgres", "select sales.orders.id from sales.orders"),
        (
            "postgres",
            "select * from a, lateral (select * from b where b.aid = a.id) s",
        ),
        ("postgres", "select * from generate_series(1, 10)"),
        (
            "postgres",
            "select distinct on (uid) uid, ts from a order by uid, ts desc",
        ),
        ("trino", "select * from hive.sales.a"),
        ("trino", "select * from a tablesample bernoulli (10)"),
        ("trino", "select x from a cross join unnest(a.arr) as t(x)"),
        ("trino", "select * from unnest(array[1,2]) with ordinality"),
        ("trino", "select k, sum(v) from a group by cube (k)"),
        ("starrocks", "select * from table(generator(10))"),
        (
            "starrocks",
            "select id, row_number() over (partition by k order by ts) rn from a qualify rn = 1",
        ),
        ("starrocks", "select /*+ SET_VAR(query_timeout=5) */ * from a"),
    ];
    for (dialect, sql) in corpus {
        let noop = apply_row_filter(sql, "zzq = 1", &options(dialect).table_names(["zzq_missing"]))
            .unwrap_or_else(|e| panic!("[{dialect}] {sql}: {e}"));
        assert_eq!(noop, sql, "no-op changed input");

        let out =
            apply_row_filter(sql, "zzq = 1", &options(dialect)).unwrap_or_else(|e| panic!("[{dialect}] {sql}: {e}"));
        parse_one(&out, dialect);
        if out != sql {
            let base = table_counts(sql, dialect).unwrap();
            assert_eq!(
                table_counts(&out, dialect).unwrap(),
                base,
                "[{dialect}] table multiset changed:\n in: {sql}\nout: {out}"
            );
        }
    }
}
