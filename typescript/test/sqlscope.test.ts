import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { Worker } from "node:worker_threads";

import * as sqlscope from "sqlscope-rs";
import { type ColumnUsage, type TableRewrite } from "sqlscope-rs";

test("applyRowFilter", () => {
  const out = sqlscope.applyRowFilter("SELECT id FROM orders", "tenant_id = 7", { dialect: "postgres" });
  assert.equal(out, "SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders");
});

test("applyRowFilter scope", () => {
  const sql = "SELECT * FROM a JOIN c ON a.id = c.id";
  assert.equal(sqlscope.applyRowFilter(sql, "t = 1", { tableNames: ["a"] }).split("t = 1").length, 2);
  assert.equal(sqlscope.applyRowFilter(sql, "t = 1", { tablePatterns: "^zz$" }), sql);
  const out = sqlscope.applyRowFilter("SELECT * FROM a", "t = 1", { tableNames: new Set(["db.a"]), defaultDb: "db" });
  assert.match(out, /WHERE t = 1/);
});

test("injectCtes", () => {
  const out = sqlscope.injectCtes("SELECT * FROM foo", [
    { name: "foo", query: "SELECT id FROM t" },
    ["bar", "SELECT 1"],
  ]);
  assert.equal(out, "WITH foo AS (SELECT id FROM t), bar AS (SELECT 1) SELECT * FROM foo");
});

test("rewriteTables", () => {
  const inline: TableRewrite = {
    matchKey: "myschema.mytable",
    inline: { table: "view1", schema: "vsch", catalog: "cat" },
  };
  assert.equal(
    sqlscope.rewriteTables("SELECT col1 FROM myschema.mytable", [inline]),
    "SELECT col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable",
  );

  const union: TableRewrite = {
    matchKey: "myschema.mytable",
    union: {
      tableAlias: "replacement",
      columns: ["col1"],
      branches: [
        { table: "v1", schema: "s" },
        { table: "v2", schema: "s" },
      ],
    },
  };
  assert.equal(
    sqlscope.rewriteTables("SELECT mytable.col1 FROM hive.myschema.mytable", [union], { stripCatalogs: ["hive"] }),
    "SELECT replacement.col1 FROM (SELECT col1 FROM s.v1 UNION DISTINCT SELECT col1 FROM s.v2) AS replacement",
  );
});

test("rewriteTables requires one target", () => {
  assert.throws(() => sqlscope.rewriteTables("SELECT 1", [{ matchKey: "s.t" }]), sqlscope.InvalidArgumentError);
});

test("columnOrigins", () => {
  assert.deepEqual(
    sqlscope.columnOrigins("SELECT * FROM hive.raw.orders o WHERE o.status = 'X'", {
      schema: { "hive.raw.orders": ["id", "status"] },
    }),
    { "hive.raw.orders": ["id", "status"] },
  );
  assert.deepEqual(sqlscope.columnOrigins("SELECT a FROM t WHERE b > 1"), { t: ["a"] });
});

test("outputColumns", () => {
  assert.deepEqual(sqlscope.outputColumns("SELECT id, amount AS total, count(*) FROM orders"), ["id", "total", "_col2"]);
  assert.deepEqual(sqlscope.outputColumns("SELECT * FROM t", { schema: new Map([["t", ["a", "b"]]]) }), ["a", "b"]);
  assert.equal(sqlscope.outputColumns("DROP TABLE t"), null);
});

test("referencedColumns and columnUsages", () => {
  const sql = "SELECT id FROM orders WHERE status = 'PAID'";
  assert.deepEqual(sqlscope.referencedColumns(sql), { orders: ["id", "status"] });
  const usages: ColumnUsage[] = sqlscope.columnUsages(sql);
  assert.deepEqual(usages, [
    { table: "orders", column: "id", clause: "SELECT" },
    { table: "orders", column: "status", clause: "WHERE" },
  ]);
});

test("errors", () => {
  const cases: [() => unknown, new (...args: never[]) => sqlscope.SqlscopeError][] = [
    [() => sqlscope.applyRowFilter("SELECT * FRM", "x = 1"), sqlscope.ParseError],
    [() => sqlscope.applyRowFilter("DELETE FROM t", "x = 1"), sqlscope.UnsupportedError],
    [() => sqlscope.applyRowFilter("SELECT 1", ""), sqlscope.InvalidArgumentError],
    [() => sqlscope.referencedColumns("SELECT 1", { dialect: "nope" }), sqlscope.InvalidArgumentError],
    [() => sqlscope.columnOrigins("SELECT 1; SELECT 2"), sqlscope.UnsupportedError],
  ];
  for (const [call, error] of cases) {
    assert.throws(call, (thrown: unknown) => {
      assert.ok(thrown instanceof error, String(thrown));
      assert.ok(thrown instanceof sqlscope.SqlscopeError);
      assert.ok(thrown instanceof Error);
      return true;
    });
  }
});

test("unicode", () => {
  assert.deepEqual(sqlscope.outputColumns('SELECT "ünï", \'😀\' AS e FROM t'), ["ünï", "e"]);
});

test("deep nesting", () => {
  let sql = "SELECT a FROM t";
  for (let i = 0; i < 120; i++) sql = `SELECT a FROM (${sql}) x${i}`;
  assert.deepEqual(sqlscope.columnOrigins(sql), { t: ["a"] });
});

test("version", async () => {
  const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as { version: string };
  assert.equal(sqlscope.version(), pkg.version);
  await sqlscope.init(); // already loaded: no-op
});

test("worker threads", async () => {
  const source = `
    import { workerData, parentPort } from "node:worker_threads";
    const { referencedColumns } = await import(workerData.module);
    parentPort.postMessage(referencedColumns(\`SELECT a\${workerData.i} FROM t\`).t);
  `;
  const module = import.meta.resolve("sqlscope-rs");
  const results = await Promise.all(
    Array.from({ length: 4 }, (_, i) => {
      const worker = new Worker(new URL(`data:text/javascript,${encodeURIComponent(source)}`), { workerData: { module, i } });
      return new Promise((resolve, reject) => {
        worker.once("message", resolve);
        worker.once("error", reject);
      });
    }),
  );
  assert.deepEqual(results, [["a0"], ["a1"], ["a2"], ["a3"]]);
});

test("require", async () => {
  const { createRequire } = await import("node:module");
  const required = createRequire(import.meta.url)("sqlscope-rs") as typeof sqlscope;
  assert.deepEqual(required.outputColumns("SELECT a FROM t"), ["a"]);
});
