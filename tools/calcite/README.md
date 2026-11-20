# Apache Calcite conversion tests

The pinned [RelToSqlConverterTest.java](https://github.com/apache/calcite/blob/c01f6b5519d4a906cf41bbe7843f5929662cf5fd/core/src/test/java/org/apache/calcite/rel/rel2sql/RelToSqlConverterTest.java)
contains 32 Trino renderings of relational plans. The exporter takes each
rendering as its input and pairs it with each MySQL or StarRocks expectation
from the same assertion chain. It preserves all 25 target assertions, including
repeated inputs from distinct plans. The 39 other combinations have separately
labelled SQLGlot v30.33.0 reference results. Reviewed alternatives document
equivalent syntax and known target limitations. Strict-mode assertions check
the error class. SQL assertions retain token types, quoted values and order.

Reviewed alternatives cover a single constant input row for PI and MAP, the
StarRocks MAP function, and CASE expressions for NULL ordering. An ordinal
referencing COUNT(*) can rank the count itself. MySQL uses WITH ROLLUP and
strict mode rejects GROUP BY CUBE and APPROX_DISTINCT. Original expectations
remain in the fixture. The Python SDK suite also runs Calcite's
[OFFSET/LIMIT example](https://github.com/apache/calcite/blob/c01f6b5519d4a906cf41bbe7843f5929662cf5fd/core/src/test/resources/sql/sort.iq#L230)
on SQLite using the generated common SQL and compares the ordered rows.

Run `cargo test -p sqlscope --test calcite_transpile` offline. To regenerate,
check out Apache Calcite 1.42.0, install SQLGlot 30.33.0 in a Python environment
and run `python tools/calcite/export.py /path/to/calcite`. Apache Calcite's
LICENSE and NOTICE accompany the extracted test data.
