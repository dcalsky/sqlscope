# sqlscope

Scope-aware SQL analysis and rewriting for Rust, Python and Go, built on the
[polyglot](https://github.com/tobilg/polyglot) SQL engine (30+ dialects).

| Operation | Purpose |
| --- | --- |
| `apply_row_filter` | Filter every in-scope table by a predicate (row-level security). |
| `inject_ctes` | Prepend CTE definitions to a query's root `WITH`. |
| `rewrite_tables` | Replace table references with derived tables. |
| `column_origins` | Source columns whose values reach the result (column lineage). |
| `output_columns` | Names of the columns a statement outputs. |
| `referenced_columns` | Columns referenced anywhere (filters included), per table. |
| `column_usages` | Column references together with the clause they appear in. |

Everything works on the parsed AST, never by splicing SQL text, and resolves
names with SQL scoping rules: CTE shadowing, correlated subqueries, set
operations, derived tables, `USING`, `PIVOT`, `UNNEST`, lateral views.

## Install

| Language | Package |
| --- | --- |
| Rust | `cargo add sqlscope` |
| Python | `pip install sqlscope` (wheels for Linux, macOS, Windows; Python ≥ 3.9) |
| Go | `go get github.com/dcalsky/sqlscope/go` (pure Go, no cgo) |

## Usage

### Rust

```rust
use sqlscope::{apply_row_filter, column_origins, Dialect, Options};

let sql = apply_row_filter(
    "SELECT id FROM orders",
    "tenant_id = 7",
    &Options::new().dialect(Dialect::POSTGRES),
)?;
assert_eq!(sql, "SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders");

let origins = column_origins(
    "SELECT o.id, p.amount FROM orders o JOIN payments p ON o.id = p.order_id WHERE o.status = 'PAID'",
    &Options::new(),
)?;
assert_eq!(origins["orders"], ["id"]);
assert_eq!(origins["payments"], ["amount"]);
```

### Python

```python
import sqlscope

sqlscope.apply_row_filter("SELECT id FROM orders", "tenant_id = 7", dialect="postgres")
# 'SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders'

sqlscope.column_usages("SELECT id FROM orders WHERE status = 'PAID'")
# [ColumnUsage(table='orders', column='id', clause=<Clause.SELECT>),
#  ColumnUsage(table='orders', column='status', clause=<Clause.WHERE>)]
```

### Go

```go
import "github.com/dcalsky/sqlscope/go"

sql, err := sqlscope.ApplyRowFilter(
    "SELECT id FROM orders", "tenant_id = 7",
    sqlscope.WithDialect("postgres"),
    sqlscope.WithTableNames("orders"),
)

cols, err := sqlscope.ColumnOrigins(sql,
    sqlscope.WithSchema(map[string][]string{"orders": {"id", "tenant_id"}}))
```

The Go SDK runs the Rust engine compiled to WebAssembly with
[wazero](https://wazero.io). The engine is compiled once per process on first
use (about a second); call `sqlscope.Init()` at startup to do it eagerly.

## Options

Every operation takes the same options. Settings an operation does not read
are ignored.

| Rust (`Options::`) | Python keyword | Go | Read by |
| --- | --- | --- | --- |
| `dialect` | `dialect` | `WithDialect` | all (default `trino`) |
| `schema` | `schema` | `WithSchema` | `column_origins`, `output_columns`, `referenced_columns`, `column_usages` |
| `table_names` | `table_names` | `WithTableNames` | `apply_row_filter` |
| `table_patterns` | `table_patterns` | `WithTableRegexp` | `apply_row_filter` |
| `default_db` | `default_db` | `WithDefaultDB` | `apply_row_filter` |
| `strip_catalogs` | `strip_catalogs` | `WithStripCatalogs` | `rewrite_tables` |

`schema` maps table names (bare, `schema.table` or `catalog.schema.table`) to
their ordered columns. It expands `*` and attributes unqualified columns. A
table listed with no columns has zero columns; an unlisted table is unknown.

## Operations

### `apply_row_filter(sql, predicate)`

```text
SELECT * FROM a  ->  SELECT * FROM (SELECT * FROM a WHERE <predicate>) AS a
```

Wraps every in-scope physical table of a `SELECT` or set operation in a
filtered derived table, which keeps the filter correct across outer joins,
CTEs, subqueries and set operations. CTE references, table functions and
`DUAL` are never wrapped. The predicate is parsed as one boolean expression
(extra clauses, statements or unbalanced parentheses are rejected); bind or
escape its values before calling. Qualified column references
(`schema.table.col`) are rebound onto the derived-table alias, and
same-named tables get distinct aliases (`s1_t`, `s2_t`).

### `inject_ctes(sql, ctes)`

Adds `name AS (query)` definitions before the query's existing CTEs, in order,
so each may use earlier ones. Names that collide with another definition or
an existing root CTE are rejected.

### `rewrite_tables(sql, rewrites)`

Replaces references to a `schema.table` match key with
`(SELECT * FROM target)` or a `UNION DISTINCT` over several tables, keeping
the reference's alias and rebinding qualified columns.

### `column_origins(sql)`

The source columns whose **values** reach the result, keyed by root table.
Filter-only positions (`WHERE`, `JOIN ... ON`, `GROUP BY`, `HAVING`, top-level
`ORDER BY`) and the right side of `INTERSECT`/`EXCEPT` are excluded; window
`PARTITION BY`/`ORDER BY` count. Accepts `CREATE VIEW`, `CREATE TABLE AS`,
`INSERT ... SELECT` and friends, and the assigned values of `UPDATE`/`MERGE`.

### `output_columns(sql)`

Output column names in order. Unaliased expressions are `_col{i}`; `*`
expands from the schema or stays `"*"`. Explicit column lists on views,
`CREATE TABLE` and `INSERT` win. `CREATE TABLE (... LIKE t ...)` needs `t`'s
schema. Returns none for statements without columns.

### `referenced_columns(sql)` / `column_usages(sql)`

Every column a statement reads, anywhere, including `DELETE`/`UPDATE`/`MERGE`
— a superset of `column_origins`. `column_usages` adds the clause
(`SELECT`, `WHERE`, `JOIN_ON`, `GROUP_BY`, `ORDER_BY`, `UPDATE_SET_VALUE`,
`MERGE_ON`, ...). Projection aliases and ordinals (`ORDER BY 1`) resolve to
their source columns. Resolution fails open: a reference that cannot be
attributed precisely is attributed to every candidate table, never dropped.

## Errors

| Kind | Rust `ErrorKind` | Python | Go |
| --- | --- | --- | --- |
| Invalid option or argument | `InvalidArgument` | `InvalidArgumentError` | `ErrInvalidArgument` |
| SQL does not parse | `Parse` | `ParseError` | `ErrParse` |
| Unsupported statement or input over a safety limit | `Unsupported` | `UnsupportedError` | `ErrUnsupported` |
| Bug (invalid output) | `Internal` | `InternalError` | `ErrInternal` |

Input is limited to 1 MiB and 256 levels of parser nesting on every platform.
Rewrites regenerate SQL from the AST (normalized formatting, comments
removed, optimizer hints kept) and verify that the output parses before
returning it; when nothing needs rewriting, the input is returned unchanged.

## Migrating from easysql

| easysql (Go) | sqlscope |
| --- | --- |
| `WithDialect`, `WithBindCTEDialect`, `WithLineageDialect`, `WithRewriteDialect` | `WithDialect` (one shared `Option`) |
| `WithLineageMetadata` | `WithSchema` |
| `WithSelfCheck` | removed (output is always verified) |
| `WithLineageProducer`, `WithLineageNamespace` | removed (did not affect results) |
| `StripMatchCatalogs` | `WithStripCatalogs` |
| `BindCTEs` / `CTEBinding` | `InjectCTEs` / `CTEDef` |
| `RewriteTableReferences` | `RewriteTables` |
| `LineageSourceColumns`, `LineageSourceColumnsConcurrent` | `ColumnOrigins` |
| `ParseColumns` | `OutputColumns` |
| `ReferencedColumnUsages` / `ColumnUse` | `ColumnUsages` / `ColumnUsage` |
| `ApplyRowFilter`, `ReferencedColumns` | unchanged |

Behavior changes: the default dialect is `trino` for every operation
(`ApplyRowFilter` previously defaulted to `mysql`); every polyglot dialect is
accepted; invalid table names in `WithTableNames` are errors instead of being
ignored; and errors also carry `ErrInvalidArgument` for configuration
mistakes.

## Development

```bash
cargo test --workspace --exclude sqlscope-python     # Rust
cd python && uv venv && uv pip install maturin pytest \
  && .venv/bin/maturin develop && .venv/bin/pytest    # Python
scripts/build-wasm.sh && (cd go && go test ./...)     # Go
```

`scripts/build-wasm.sh` needs the `wasm32-wasip1` Rust target and regenerates
`go/internal/wasm/sqlscope.wasm.gz`, which is committed so `go get` works; CI
checks that it matches the sources.

## License

MIT
