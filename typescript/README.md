# sqlscope-rs

Scope-aware SQL analysis and rewriting for TypeScript and JavaScript: row
filters, CTE injection, table rewrites and column lineage over 30+ SQL
dialects. It runs [sqlscope](https://github.com/dcalsky/sqlscope) (Rust, built
on the [polyglot](https://github.com/tobilg/polyglot) SQL engine) as a
WebAssembly module bundled in the package: no native library, no install
scripts.

```bash
npm install sqlscope-rs
```

| Function | Purpose |
| --- | --- |
| `applyRowFilter` | Filter every in-scope table by a predicate (row-level security). |
| `injectCtes` | Prepend CTE definitions to a query's root `WITH`. |
| `rewriteTables` | Replace table references with derived tables. |
| `columnOrigins` | Source columns whose values reach the result (column lineage). |
| `outputColumns` | Names of the columns a statement outputs. |
| `referencedColumns` | Columns referenced anywhere (filters included), per table. |
| `columnUsages` | Column references together with the clause they appear in. |

## Usage

```ts
import { applyRowFilter, columnOrigins, columnUsages, injectCtes, rewriteTables } from "sqlscope-rs";

applyRowFilter("SELECT id FROM orders", "tenant_id = 7", { dialect: "postgres" });
// 'SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders'

applyRowFilter("SELECT * FROM a JOIN b ON a.id = b.id", "tenant_id = 7", { tableNames: ["a"] });

columnOrigins(
  "SELECT o.id, p.amount FROM orders o JOIN payments p ON o.id = p.order_id WHERE o.status = 'PAID'",
);
// { orders: ['id'], payments: ['amount'] }

columnOrigins("SELECT * FROM orders", { schema: { orders: ["id", "status"] } });
// { orders: ['id', 'status'] }

columnUsages("SELECT id FROM orders WHERE status = 'PAID'");
// [{ table: 'orders', column: 'id', clause: 'SELECT' },
//  { table: 'orders', column: 'status', clause: 'WHERE' }]

injectCtes("SELECT * FROM foo", [{ name: "foo", query: "SELECT id FROM t" }]);
// 'WITH foo AS (SELECT id FROM t) SELECT * FROM foo'

rewriteTables("SELECT col1 FROM myschema.mytable", [
  { matchKey: "myschema.mytable", inline: { table: "view1", schema: "vsch" } },
]);
// 'SELECT col1 FROM (SELECT * FROM vsch.view1) AS mytable'
```

Every function is synchronous. Options:

| Option | Read by |
| --- | --- |
| `dialect` | all (default `"trino"`; `"postgres"`, `"mysql"`, `"spark"`, `"snowflake"`, `"bigquery"`, ...) |
| `schema` | `columnOrigins`, `outputColumns`, `referencedColumns`, `columnUsages` |
| `tableNames`, `tablePatterns`, `defaultDb` | `applyRowFilter` |
| `stripCatalogs` | `rewriteTables` |

`schema` maps table names (bare, `schema.table` or `catalog.schema.table`) to
their ordered columns, as an object or a `Map`.

Errors are thrown as `InvalidArgumentError`, `ParseError`, `UnsupportedError`
or `InternalError`, all subclasses of `SqlscopeError`.

The [sqlscope README](https://github.com/dcalsky/sqlscope#operations)
describes each operation in detail.

## Runtimes

**Node.js (≥ 20.19), Deno and Bun** load the WebAssembly module when the
package is imported (`import` or `require`); just call the functions.

**Browsers, workers and edge runtimes** load it asynchronously: `await init()`
once before calling anything else.

```ts
import { init, outputColumns } from "sqlscope-rs";

await init(); // fetches sqlscope.wasm next to the package
outputColumns("SELECT a, b FROM t");
```

`init()` resolves `sqlscope.wasm` with `new URL(..., import.meta.url)`, which
webpack, Rollup and esbuild handle. With Vite, import the module's URL
explicitly:

```ts
import { init } from "sqlscope-rs";
import wasmUrl from "sqlscope-rs/sqlscope.wasm?url";

await init(wasmUrl);
```

`init` also accepts a `Response`, the bytes, or a compiled
`WebAssembly.Module`; `initSync(bytes)` loads synchronously. Calling `init()`
under Node.js is harmless, so the same code runs everywhere.

The module is about 9 MB (2.5 MB gzipped). V8 compiles it lazily; under
sustained load its optimizing compiler can hold several hundred MB of
optimized code. Run Node.js with `--liftoff-only` to trade speed for memory.

## License

MIT
