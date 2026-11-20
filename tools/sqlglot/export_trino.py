"""Export Trino inputs and target contracts from the pinned SQLGlot unit suite."""

import json
import os
import subprocess
import sys
import unittest
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch

REVISION = "0936dbe9192a3b57c6e6992acfcb93d4b429c51f"
DEST = Path(__file__).resolve().parents[2] / "crates/sqlscope/tests/fixtures/sqlglot/trino.json"
TARGETS = ("mysql", "starrocks")


def main():
    upstream = Path(sys.argv[1]).resolve()
    os.chdir(upstream)
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if head != REVISION or subprocess.check_output(["git", "diff", "HEAD", "--", "tests", "sqlglot"]):
        raise SystemExit("Expected an unmodified SQLGlot v30.33.0 checkout")
    sys.path.insert(0, str(upstream))
    import sqlglot
    from sqlglot import ErrorLevel, ParseError, TokenError, UnsupportedError
    from sqlglot.dialects import MySQL, StarRocks, Trino
    from tests.helpers import load_sql_fixture_pairs
    from tests import test_optimizer
    from tests.dialects.test_dialect import Validator

    inputs = defaultdict(set)
    explicit = []
    roots = {}
    direct_calls = set()
    test_id = None

    class Result(unittest.TextTestResult):
        def startTest(self, test):
            nonlocal test_id
            roots.clear()
            test_id = test.id()
            super().startTest(test)

    tokenize = Trino.tokenize

    def record_input(self, sql, *args, **kwargs):
        if type(self) is Trino:
            inputs[sql].add(test_id)
        return tokenize(self, sql, *args, **kwargs)

    def track_parse(original, parse_into=False):
        def tracked(self, *args, **kwargs):
            result = original(self, *args, **kwargs)
            if type(self) is Trino:
                sql = kwargs["sql"] if "sql" in kwargs else args[int(parse_into)]
                for root in result:
                    if root is not None:
                        roots[id(root)] = (root, sql)
            return result
        return tracked

    def track_generate(original, dialect):
        def tracked(self, expression, *args, **kwargs):
            if type(self) is dialect and id(expression) in roots:
                direct_calls.add((test_id, roots[id(expression)][1], dialect.__name__.lower()))
            return original(self, expression, *args, **kwargs)
        return tracked

    def record(source, target, expected, pretty=False, identify=False):
        unsupported = expected is UnsupportedError
        outcome = {"error": "UnsupportedError"} if unsupported else {"sql": [expected]}
        explicit.append(dict(test=test_id, input=source, target=target, pretty=pretty,
                             identify=identify, strict=unsupported, expected=outcome))

    validate_all = Validator.validate_all
    validate_transpile = Validator.validate_transpile

    def record_all(self, sql, read=None, write=None, pretty=False, identify=False):
        if self.dialect in TARGETS and "trino" in (read or {}):
            record(read["trino"], self.dialect, sql, pretty, identify)
        if self.dialect == "trino":
            for target, expected in (write or {}).items():
                if target in TARGETS:
                    record(sql, target, expected, pretty, identify)
        return validate_all(self, sql, read, write, pretty, identify)

    def record_transpile(self, sql, write_sql, write_dialect=None):
        if self.dialect == "trino" and write_dialect in TARGETS:
            record(sql, write_dialect, write_sql)
        return validate_transpile(self, sql, write_sql, write_dialect)

    suite = unittest.defaultTestLoader.discover("tests", top_level_dir=".")
    with ExitStack() as stack:
        # Run optimizer workers in this interpreter so tokenizer events from
        # their input files and any nested parsing carry the originating test ID.
        stack.enter_context(patch.object(test_optimizer, "ProcessPoolExecutor",
                                         lambda **_: ThreadPoolExecutor(max_workers=1)))
        stack.enter_context(patch.object(Trino, "tokenize", record_input))
        stack.enter_context(patch.object(Trino, "parse", track_parse(Trino.parse)))
        stack.enter_context(patch.object(Trino, "parse_into", track_parse(Trino.parse_into, True)))
        for dialect in (MySQL, StarRocks):
            stack.enter_context(patch.object(dialect, "generate", track_generate(dialect.generate, dialect)))
        stack.enter_context(patch.object(Validator, "validate_all", record_all))
        stack.enter_context(patch.object(Validator, "validate_transpile", record_transpile))
        result = unittest.TextTestRunner(resultclass=Result, verbosity=0).run(suite)
    if not result.wasSuccessful() or result.skipped:
        raise SystemExit("Upstream tests failed or skipped; refusing to update the corpus")

    covered = {(c["test"], c["input"], c["target"]) for c in explicit}
    if missing := direct_calls - covered:
        raise SystemExit(f"Audit additional direct Trino-to-target calls: {sorted(missing)}")

    # Type-annotation tests can wrap a fixture expression in a SELECT. Keep
    # both the fixture as authored and the complete input observed at runtime.
    for fixture in sorted(Path("tests/fixtures/optimizer").glob("*.sql")):
        if "# dialect:" not in fixture.read_text():
            continue
        for meta, sql, _ in load_sql_fixture_pairs(str(fixture.relative_to("tests/fixtures"))):
            if "trino" in [d.strip() for d in meta.get("dialect", "").split(",")]:
                inputs[sql].add(str(fixture))

    def expected(sql, target, strict):
        try:
            return {"sql": sqlglot.transpile(sql, read="trino", write=target,
                        unsupported_level=ErrorLevel.RAISE if strict else ErrorLevel.IGNORE)}
        except (ParseError, TokenError, UnsupportedError) as error:
            return {"error": type(error).__name__}

    cases = []
    for sql, tests in sorted(inputs.items()):
        targets = {
            target: {mode: expected(sql, target, mode == "strict") for mode in ("default", "strict")}
            for target in TARGETS
        }
        cases.append(dict(input=sql, tests=sorted(tests), targets=targets))
    data = dict(revision=REVISION, upstream_tests=result.testsRun, explicit=explicit, cases=cases)
    DEST.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
    print(f"{len(explicit)} explicit target assertions, {len(cases)} distinct Trino inputs")


if __name__ == "__main__":
    main()
