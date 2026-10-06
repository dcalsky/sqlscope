/**
 * Scope-aware SQL analysis and rewriting.
 *
 * | Function | Purpose |
 * | --- | --- |
 * | {@link applyRowFilter} | Filter every in-scope table by a predicate. |
 * | {@link injectCtes} | Prepend CTE definitions to a query's root `WITH`. |
 * | {@link rewriteTables} | Replace table references with derived tables. |
 * | {@link columnOrigins} | Source columns whose values reach the result (lineage). |
 * | {@link outputColumns} | Names of the columns a statement outputs. |
 * | {@link referencedColumns} | Columns referenced anywhere, per table. |
 * | {@link columnUsages} | Column references with the clause they appear in. |
 *
 * The SQL engine is a WebAssembly module shipped in this package. Node.js,
 * Deno and Bun load it on import; browsers and other runtimes `await init()`
 * first.
 *
 * @module
 */

export * from "./api.js";
export {
  InternalError,
  InvalidArgumentError,
  ParseError,
  SqlscopeError,
  UnsupportedError,
  init,
  initSync,
  version,
} from "./engine.js";
export type { WasmBytes, WasmModule, WasmResponse, WasmSource } from "./engine.js";
