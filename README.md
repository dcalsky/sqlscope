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

Each [release](https://github.com/dcalsky/sqlscope/releases) ships the
sqlscope FFI library for Linux (x86_64, aarch64; glibc ≥ 2.35), macOS
(x86_64, aarch64) and Windows (x86_64) as `sqlscope-ffi-<platform>.tar.gz`
/ `.zip`: a shared library (`libsqlscope_ffi.so`, `libsqlscope_ffi.dylib`,
`sqlscope_ffi.dll`), a static library (`libsqlscope_ffi.a`,
`sqlscope_ffi.lib`) and the C header `sqlscope.h`.

The Python and Go SDKs are thin wrappers that load that shared library at
runtime. They never bundle or download it: install the library yourself and
point the SDK at it with `SQLSCOPE_LIBRARY_PATH`, or with `sqlscope.load(path)`
(Python) / `sqlscope.Load(path)` (Go). Without either, the SDKs look for the
library in the Python package directory (Python) or next to the executable
(Go), then on the system library search path.

| Language | Package |
| --- | --- |
| Rust | `cargo add sqlscope` |
| C / C++ / others | link `sqlscope-ffi` from a release; see `sqlscope.h` |
| Python | `pip install sqlscope-rs` (imported as `sqlscope`; pure Python, ≥ 3.9) |
| Go | `go get github.com/dcalsky/sqlscope/go` (no cgo; uses [purego](https://github.com/ebitengine/purego)) |

Use the SDK and library from the same release; they check the ABI version
when loading.

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

### C

```c
#include "sqlscope.h"

char *response = sqlscope_call("apply_row_filter",
    "{\"sql\": \"SELECT id FROM orders\", \"predicate\": \"tenant_id = 7\"}");
/* {"ok":"SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders"} */
sqlscope_free(response);
```

Every operation takes a JSON request and returns `{"ok": ...}` or
`{"error": {"kind": ..., "message": ...}}`; `sqlscope.h` documents the
request fields. Linking the static library also needs a few system libraries,
listed in the README inside each release archive.

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

## Development

```bash
make check                     # rustfmt, clippy, go vet + Rust, feature-gate, C, Go, Python tests
make test-go FFI_DIR=target/ffi  # run an SDK against the release-profile library
make help                      # every target
```

The SDK tests build the debug FFI library and point `SQLSCOPE_LIBRARY_PATH`
at it. `make build-ffi-release` builds the size-optimized libraries that
releases ship (`target/ffi`).

To release, run `make bump-version V=X.Y.Z`, merge it to `main`, then tag:
`git tag vX.Y.Z && git push origin vX.Y.Z`. The release workflow runs CI,
builds and tests the FFI library on every platform, attaches the archives
and checksums to a GitHub release, publishes the pure-Python package to PyPI
and tags the Go module as `go/vX.Y.Z`.

## License

MIT
