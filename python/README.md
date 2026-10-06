# sqlscope (Python)

Scope-aware SQL analysis and rewriting, backed by the
[sqlscope](https://github.com/dcalsky/sqlscope) FFI library.

This package is pure Python. It loads the sqlscope shared library
(`libsqlscope_ffi.so`, `libsqlscope_ffi.dylib` or `sqlscope_ffi.dll`) from a
[sqlscope release](https://github.com/dcalsky/sqlscope/releases) through
`ctypes`, and never bundles or downloads it. Point it at the library with
`SQLSCOPE_LIBRARY_PATH` or `sqlscope.load(path)`; otherwise it looks in the
package directory and then on the system library search path.

```bash
pip install sqlscope-rs
```

```python
import sqlscope

sqlscope.load("/opt/sqlscope/libsqlscope_ffi.so")  # optional with SQLSCOPE_LIBRARY_PATH

sqlscope.apply_row_filter("SELECT id FROM orders", "tenant_id = 7", dialect="postgres")
# 'SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders'

sqlscope.column_origins(
    "SELECT o.id, p.amount FROM orders o JOIN payments p ON o.id = p.order_id WHERE o.status = 'PAID'"
)
# {'orders': ['id'], 'payments': ['amount']}
```

See the [project README](https://github.com/dcalsky/sqlscope#readme) for the
full API.
