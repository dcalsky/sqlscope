# Trino conversion corpus

The reference is the pure-Python unit suite in SQLGlot **v30.33.0**, commit
`0936dbe9192a3b57c6e6992acfcb93d4b429c51f`.
[Upstream source and tests](https://github.com/tobymao/sqlglot/tree/0936dbe9192a3b57c6e6992acfcb93d4b429c51f/tests)

The exporter runs all 1,718 tests from that checkout. It captures the input
strings passed to the Trino tokenizer, including calls from other dialects'
tests and optimizer workers. It also reads Trino-tagged optimizer expressions
in their original form. The fixture records the originating test or file.

* Four direct Trino-to-MySQL assertions have target text supplied by the upstream
  test author. The exporter also audits calls that render a parsed Trino root
  directly to MySQL or StarRocks during a test.
* For all 448 distinct inputs, SQLGlot generates the expectations for both
  target dialects at `ErrorLevel.IGNORE` and `ErrorLevel.RAISE`. Rust compares
  the full output list or exception class in all 1,792 combinations.

Run the checked-in tests offline with `cargo test -p sqlscope --test trino_transpile`.
To regenerate, use a fresh Python 3.12 environment and run from the sqlscope
repository root. The last command replaces the fixture after the upstream suite passes.

```sh
git clone --depth 1 --branch v30.33.0 https://github.com/tobymao/sqlglot.git /tmp/sqlglot-v30.33.0
python -m pip install -e '/tmp/sqlglot-v30.33.0[dev]'
python tools/sqlglot/export_trino.py /tmp/sqlglot-v30.33.0
```

This contract covers the public upstream unit suite and output/error behavior.
Engine versions, session settings, column types and data can affect actual
query results. The upstream MIT license accompanies the JSON fixture.
