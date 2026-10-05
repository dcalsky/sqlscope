import threading

import pytest

import sqlscope
from sqlscope import Clause, ColumnUsage, CteDef, TableRef, TableRewrite, UnionRewrite


def test_apply_row_filter():
    out = sqlscope.apply_row_filter("SELECT id FROM orders", "tenant_id = 7", dialect="postgres")
    assert out == "SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders"


def test_apply_row_filter_scope():
    sql = "SELECT * FROM a JOIN c ON a.id = c.id"
    out = sqlscope.apply_row_filter(sql, "t = 1", table_names=["a"])
    assert out.count("t = 1") == 1
    assert sqlscope.apply_row_filter(sql, "t = 1", table_patterns="^zz$") == sql
    out = sqlscope.apply_row_filter("SELECT * FROM a", "t = 1", table_names=["db.a"], default_db="db")
    assert "WHERE t = 1" in out


def test_inject_ctes():
    out = sqlscope.inject_ctes("SELECT * FROM foo", [CteDef("foo", "SELECT id FROM t"), ("bar", "SELECT 1")])
    assert out == "WITH foo AS (SELECT id FROM t), bar AS (SELECT 1) SELECT * FROM foo"


def test_rewrite_tables():
    inline = TableRewrite("myschema.mytable", inline=TableRef("view1", schema="vsch", catalog="cat"))
    out = sqlscope.rewrite_tables("SELECT col1 FROM myschema.mytable", [inline])
    assert out == "SELECT col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable"

    union = TableRewrite(
        "myschema.mytable",
        union=UnionRewrite("replacement", ["col1"], [TableRef("v1", "s"), TableRef("v2", "s")]),
    )
    out = sqlscope.rewrite_tables("SELECT mytable.col1 FROM hive.myschema.mytable", [union], strip_catalogs=["hive"])
    assert out == (
        "SELECT replacement.col1 FROM (SELECT col1 FROM s.v1 UNION DISTINCT SELECT col1 FROM s.v2) AS replacement"
    )


def test_rewrite_tables_requires_one_target():
    with pytest.raises(sqlscope.InvalidArgumentError):
        sqlscope.rewrite_tables("SELECT 1", [TableRewrite("s.t")])


def test_column_origins():
    got = sqlscope.column_origins(
        "SELECT * FROM hive.raw.orders o WHERE o.status = 'X'",
        schema={"hive.raw.orders": ("id", "status")},
    )
    assert got == {"hive.raw.orders": ["id", "status"]}
    assert sqlscope.column_origins("SELECT a FROM t WHERE b > 1") == {"t": ["a"]}


def test_output_columns():
    assert sqlscope.output_columns("SELECT id, amount AS total, count(*) FROM orders") == ["id", "total", "_col2"]
    assert sqlscope.output_columns("SELECT * FROM t", schema={"t": ["a", "b"]}) == ["a", "b"]
    assert sqlscope.output_columns("DROP TABLE t") is None


def test_referenced_columns_and_usages():
    sql = "SELECT id FROM orders WHERE status = 'PAID'"
    assert sqlscope.referenced_columns(sql) == {"orders": ["id", "status"]}
    usages = sqlscope.column_usages(sql)
    assert usages == [
        ColumnUsage("orders", "id", Clause.SELECT),
        ColumnUsage("orders", "status", Clause.WHERE),
    ]
    assert usages[1].clause == "WHERE"


@pytest.mark.parametrize(
    "call, error",
    [
        (lambda: sqlscope.apply_row_filter("SELECT * FRM", "x = 1"), sqlscope.ParseError),
        (lambda: sqlscope.apply_row_filter("DELETE FROM t", "x = 1"), sqlscope.UnsupportedError),
        (lambda: sqlscope.apply_row_filter("SELECT 1", ""), sqlscope.InvalidArgumentError),
        (lambda: sqlscope.referenced_columns("SELECT 1", dialect="nope"), sqlscope.InvalidArgumentError),
        (lambda: sqlscope.column_origins("SELECT 1; SELECT 2"), sqlscope.UnsupportedError),
    ],
)
def test_errors(call, error):
    with pytest.raises(error) as excinfo:
        call()
    assert isinstance(excinfo.value, sqlscope.Error)


def test_threads():
    results = []

    def work(i):
        results.append(sqlscope.referenced_columns(f"SELECT a{i} FROM t")["t"])

    threads = [threading.Thread(target=work, args=(i,)) for i in range(8)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert sorted(results) == sorted([[f"a{i}"] for i in range(8)])


def test_version():
    assert sqlscope.__version__
