# sqlscope (Python)

Scope-aware SQL analysis and rewriting, backed by the Rust
[sqlscope](https://github.com/dcalsky/sqlscope) crate.

```bash
pip install sqlscope
```

```python
import sqlscope

sqlscope.apply_row_filter("SELECT id FROM orders", "tenant_id = 7", dialect="postgres")
# 'SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders'

sqlscope.column_origins(
    "SELECT o.id, p.amount FROM orders o JOIN payments p ON o.id = p.order_id WHERE o.status = 'PAID'"
)
# {'orders': ['id'], 'payments': ['amount']}
```

See the [project README](https://github.com/dcalsky/sqlscope#readme) for the
full API.
