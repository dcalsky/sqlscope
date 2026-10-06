// The sqlscope operations.

import { call } from "./engine.js";

/** Table name (bare, `schema.table` or `catalog.schema.table`) -> ordered column names. */
export type Schema = Readonly<Record<string, readonly string[]>> | ReadonlyMap<string, readonly string[]>;

/** One name, or several. */
export type Names = string | Iterable<string>;

/** Options accepted by every operation. */
export interface DialectOptions {
  /**
   * The SQL dialect: "trino" (the default), "postgres", "mysql", "starrocks",
   * "spark", "hive", "snowflake", "bigquery", "duckdb", ...
   */
  dialect?: string;
}

/** Options of the analysis operations. */
export interface SchemaOptions extends DialectOptions {
  /**
   * Table schemas. They expand wildcards and attribute unqualified columns.
   * A table listed with no columns has zero columns; an absent table is
   * unknown.
   */
  schema?: Schema;
}

/** Options of {@link applyRowFilter}. */
export interface RowFilterOptions extends DialectOptions {
  /**
   * Filter only these tables: bare (`orders`), schema-qualified
   * (`sales.orders`) or catalog-qualified (`iceberg.sales.orders`), matched
   * case-insensitively. Composes additively with `tablePatterns`.
   */
  tableNames?: Names;
  /** Filter only tables whose bare or fully qualified name matches one of these regular expressions. */
  tablePatterns?: Names;
  /** The schema of unqualified table references, matched against schema-qualified `tableNames`. */
  defaultDb?: string;
}

/** Options of {@link rewriteTables}. */
export interface RewriteOptions extends DialectOptions {
  /**
   * Catalogs that are transparent while matching: with `["hive"]`,
   * `hive.sales.orders` matches the match key `sales.orders`.
   */
  stripCatalogs?: Names;
}

/**
 * A common table expression `name AS (query)`.
 *
 * `name` is an identifier value, not SQL text; it is quoted as needed.
 */
export interface CteDef {
  name: string;
  query: string;
}

/** A table name, optionally schema- and catalog-qualified. */
export interface TableRef {
  table: string;
  schema?: string;
  catalog?: string;
}

/** A derived table backed by `UNION DISTINCT` over `branches`. */
export interface UnionRewrite {
  /** The alias used for references without an alias of their own. */
  tableAlias: string;
  columns: readonly string[];
  branches: readonly TableRef[];
}

/**
 * Replaces references to `matchKey` (`schema.table`). Set exactly one of
 * `inline`, which becomes `(SELECT * FROM inline)`, or `union`.
 */
export interface TableRewrite {
  matchKey: string;
  inline?: TableRef;
  union?: UnionRewrite;
}

/** The SQL clause that contains a column reference. */
export type Clause =
  | "SELECT"
  | "FROM"
  | "JOIN_ON"
  | "JOIN_USING"
  | "WHERE"
  | "GROUP_BY"
  | "HAVING"
  | "QUALIFY"
  | "WINDOW"
  | "ORDER_BY"
  | "SORT_BY"
  | "DISTRIBUTE_BY"
  | "CLUSTER_BY"
  | "CONNECT_BY"
  | "LATERAL_VIEW"
  | "UPDATE_SET_TARGET"
  | "UPDATE_SET_VALUE"
  | "MERGE_ON"
  | "MERGE_WHEN";

/** One distinct use of a root table column in a clause. */
export interface ColumnUsage {
  table: string;
  column: string;
  clause: Clause;
}

function names(values: Names | undefined): string[] | undefined {
  if (values === undefined) return undefined;
  return typeof values === "string" ? [values] : Array.from(values);
}

function schema(value: Schema | undefined): Record<string, readonly string[]> | undefined {
  if (value === undefined) return undefined;
  return value instanceof Map ? Object.fromEntries(value) : (value as Record<string, readonly string[]>);
}

function table(ref: TableRef): TableRef {
  return { table: ref.table, schema: ref.schema, catalog: ref.catalog };
}

/** Calls `operation`; undefined fields and options are omitted. */
function run(operation: string, sql: string, fields: object, options: object): unknown {
  return call(operation, { sql, ...fields, options });
}

/**
 * Wrap every in-scope table of a query in a derived table filtered by `predicate`.
 *
 * `SELECT * FROM a` becomes `SELECT * FROM (SELECT * FROM a WHERE <predicate>) AS a`.
 * By default every physical table is filtered; `tableNames`, `tablePatterns`
 * and `defaultDb` restrict the scope. CTE references, table functions and
 * `DUAL` are never wrapped. `predicate` is parsed as one boolean expression;
 * bind or escape its values before calling. Returns the input unchanged when
 * no table is in scope.
 */
export function applyRowFilter(sql: string, predicate: string, options: RowFilterOptions = {}): string {
  return run(
    "apply_row_filter",
    sql,
    { predicate },
    {
      dialect: options.dialect,
      tableNames: names(options.tableNames),
      tablePatterns: names(options.tablePatterns),
      defaultDb: options.defaultDb,
    },
  ) as string;
}

/**
 * Add CTE definitions to the root `WITH` of a query, before existing CTEs.
 *
 * Definitions keep their order, so each may use earlier ones. A name that
 * repeats another definition or an existing root CTE is rejected.
 */
export function injectCtes(
  sql: string,
  ctes: Iterable<CteDef | readonly [name: string, query: string]>,
  options: DialectOptions = {},
): string {
  const defs = Array.from(ctes, (cte) =>
    "name" in cte ? { name: cte.name, query: cte.query } : { name: cte[0], query: cte[1] },
  );
  return run("inject_ctes", sql, { ctes: defs }, { dialect: options.dialect }) as string;
}

/**
 * Replace physical table references according to `rewrites`.
 *
 * Matched references become derived tables that keep their alias (or bare
 * name); qualified column references are rebound onto it. Returns the input
 * unchanged when nothing matches.
 */
export function rewriteTables(sql: string, rewrites: Iterable<TableRewrite>, options: RewriteOptions = {}): string {
  const plan = Array.from(rewrites, (rewrite) => ({
    matchKey: rewrite.matchKey,
    inline: rewrite.inline && table(rewrite.inline),
    union: rewrite.union && {
      tableAlias: rewrite.union.tableAlias,
      columns: Array.from(rewrite.union.columns),
      branches: Array.from(rewrite.union.branches, table),
    },
  }));
  return run(
    "rewrite_tables",
    sql,
    { rewrites: plan },
    { dialect: options.dialect, stripCatalogs: names(options.stripCatalogs) },
  ) as string;
}

function analyze(operation: string, sql: string, options: SchemaOptions): unknown {
  return run(operation, sql, {}, { dialect: options.dialect, schema: schema(options.schema) });
}

/**
 * Source columns whose values flow into the result, keyed by root table.
 *
 * Filter-only positions (WHERE, JOIN ON, GROUP BY, HAVING, ORDER BY) and the
 * right side of INTERSECT / EXCEPT are excluded. Every table read is present,
 * possibly with an empty list.
 */
export function columnOrigins(sql: string, options: SchemaOptions = {}): Record<string, string[]> {
  return analyze("column_origins", sql, options) as Record<string, string[]>;
}

/**
 * Names of the columns a statement outputs, in order.
 *
 * Unaliased expressions are named `_col{i}`; `*` expands from `schema` or
 * stays `"*"`. Returns `null` for statements without columns.
 */
export function outputColumns(sql: string, options: SchemaOptions = {}): string[] | null {
  return analyze("output_columns", sql, options) as string[] | null;
}

/** Columns referenced anywhere in a statement (including filters), keyed by root table. */
export function referencedColumns(sql: string, options: SchemaOptions = {}): Record<string, string[]> {
  return analyze("referenced_columns", sql, options) as Record<string, string[]>;
}

/** Every distinct `(table, column, clause)` use in a statement, sorted. */
export function columnUsages(sql: string, options: SchemaOptions = {}): ColumnUsage[] {
  return analyze("column_usages", sql, options) as ColumnUsage[];
}
