//! Ported from sql-guard `easysql_invariants_test.go`. The invariants are
//! independent of the rewrite's own logic:
//!
//! 1. scoping to a table that does not exist returns the input unchanged;
//! 2. a successful wrap-all output parses;
//! 3. the multiset of physical tables is the same before and after.
//!
//! `TestNilFunctionalOptionsReturnErrors` has no counterpart: Rust options
//! cannot be nil.

use std::collections::BTreeMap;

use sqlscope::{
    apply_row_filter, column_origins, column_usages, inject_ctes, output_columns, referenced_columns, rewrite_tables,
    CteDef, ErrorKind, TableRef, TableRewrite,
};

use crate::common::*;

const MARKER: &str = "zzq_inv_marker = 1";

/// The physical-table multiset of `sql` after one parse + generate cycle,
/// the same round trip a rewrite's output goes through.
fn regenerated_table_counts(sql: &str, dialect: &str) -> Option<BTreeMap<String, usize>> {
    let polyglot = if dialect == "postgres" { "postgresql" } else { dialect };
    let polyglot: polyglot_sql::DialectType = polyglot.parse().ok()?;
    let statements = polyglot_sql::parse(sql, polyglot).ok()?;
    if statements.len() != 1 {
        return None;
    }
    let generated = polyglot_sql::generate(&statements[0], polyglot).ok()?;
    table_counts(&generated, dialect)
}

fn check_invariants(dialect: &str, sql: &str) {
    let opts = || options(dialect);
    match apply_row_filter(sql, MARKER, &opts().table_names(["zzq_table_that_does_not_exist_zzq"])) {
        Ok(out) => assert_eq!(out, sql, "[{dialect}] no-op scope mutated input"),
        Err(error) => assert_ne!(error.kind(), ErrorKind::Internal, "[{dialect}] {sql}: {error}"),
    }

    let out = match apply_row_filter(sql, MARKER, &opts()) {
        Ok(out) => out,
        Err(error) => {
            // Failing closed is acceptable; an internal error is a bug.
            assert_ne!(error.kind(), ErrorKind::Internal, "[{dialect}] {sql}: {error}");
            return;
        }
    };
    parse_one(&out, dialect);
    let base = if out == sql {
        table_counts(sql, dialect)
    } else {
        regenerated_table_counts(sql, dialect)
    };
    if let (Some(base), Some(got)) = (base, table_counts(&out, dialect)) {
        assert_eq!(base, got, "[{dialect}] table multiset changed:\n in: {sql}\nout: {out}");
    }
}

const CORPUS: &[(&str, &str)] = &[
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

#[test]
fn rewrite_invariants() {
    for (dialect, sql) in CORPUS {
        check_invariants(dialect, sql);
    }
}

#[test]
fn input_guard() {
    let deep_func = format!("select s{}1{} from a", "(".repeat(4000), ")".repeat(4000));
    let deep_unary = format!("select {}1 from a", "~ ".repeat(4000));
    let huge = format!("select * from a where x in ({}1)", "1,".repeat(1 << 20));
    for (name, sql) in [
        ("deep function nesting", &deep_func),
        ("deep unary nesting", &deep_unary),
        ("oversized input", &huge),
    ] {
        let error = apply_row_filter(sql, "t = 1", &options("mysql")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{name}: {error}");
    }
    for sql in [
        format!("select '{}' from a", "(".repeat(300)),
        format!("select /* {} */ id from a", "{".repeat(300)),
        "select * from a where note = '{a:{b:{c}}}'".to_owned(),
        "select * from a where x in (1, (2), ((3)))".to_owned(),
    ] {
        apply_row_filter(&sql, "t = 1", &options("mysql")).unwrap_or_else(|e| panic!("valid SQL rejected: {e}"));
    }
}

#[test]
fn input_guard_covers_every_public_parse_path() {
    let deep_expr = format!("{}1{}", "(".repeat(4000), ")".repeat(4000));
    let deep_sql = format!("SELECT {deep_expr} FROM s.t");
    let opts = options("trino");
    let rewrite = [TableRewrite::inline("s.t", TableRef::new("t").with_schema("safe"))];
    let checks: Vec<(&str, sqlscope::Result<()>)> = vec![
        ("column_origins", column_origins(&deep_sql, &opts).map(drop)),
        ("output_columns", output_columns(&deep_sql, &opts).map(drop)),
        ("referenced_columns", referenced_columns(&deep_sql, &opts).map(drop)),
        ("column_usages", column_usages(&deep_sql, &opts).map(drop)),
        ("rewrite_tables", rewrite_tables(&deep_sql, &rewrite, &opts).map(drop)),
        (
            "inject_ctes",
            inject_ctes(&deep_sql, &[CteDef::new("bound", "SELECT 1")], &opts).map(drop),
        ),
        (
            "apply_row_filter predicate",
            apply_row_filter("SELECT * FROM t", &deep_expr, &opts).map(drop),
        ),
    ];
    for (name, result) in checks {
        let error = result.expect_err(name);
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{name}: {error}");
    }
}

/// Ported from `easysql_bench_test.go`: the benchmark corpus rewrites to
/// valid SQL that reads the same multiset of tables.
#[test]
fn rewrite_corpus() {
    let corpus = [
        ("simple", "mysql", "select * from orders"),
        ("two_join", "mysql", "select a.id, b.name from orders a join users b on a.uid = b.id"),
        (
            "three_join",
            "mysql",
            "select * from orders o join users u on o.uid = u.id join items i on i.oid = o.id where o.ts > 0",
        ),
        ("left_join", "mysql", "select * from orders o left join users u on o.uid = u.id"),
        (
            "subquery_from",
            "mysql",
            "select * from (select * from orders) x join users u on x.uid = u.id",
        ),
        (
            "cte",
            "mysql",
            "with c as (select * from orders) select * from c join users u on c.uid = u.id",
        ),
        ("union", "mysql", "select * from orders union all select * from archived_orders"),
        (
            "scalar_subquery",
            "mysql",
            "select o.id, (select count(*) from events e where e.oid = o.id) n from orders o",
        ),
        (
            "schema_qualified",
            "postgres",
            "select sales.orders.id, u.name from sales.orders, public.users u where sales.orders.uid = u.id",
        ),
        (
            "analytical",
            "mysql",
            "select u.region, count(*) c, sum(o.amount) total from orders o join users u on o.uid = u.id join items i on i.oid = o.id where o.ts >= 100 and u.active = 1 group by u.region having sum(o.amount) > 0 order by total desc limit 10",
        ),
    ];
    for (name, dialect, sql) in corpus {
        let out = apply_row_filter(sql, "z_pt = 1", &options(dialect)).unwrap_or_else(|e| panic!("{name}: {e}"));
        parse_one(&out, dialect);
        if let (Some(input), Some(output)) = (table_counts(sql, dialect), table_counts(&out, dialect)) {
            assert_eq!(input, output, "{name}: table multiset differs\n in: {sql}\nout: {out}");
        }
    }
}
