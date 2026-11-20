"""Extract paired dialect renderings of the same relational plan from Apache Calcite."""

import json
import re
import subprocess
import sys
from pathlib import Path

import sqlglot
from sqlglot import ErrorLevel, UnsupportedError

REV = "c01f6b5519d4a906cf41bbe7843f5929662cf5fd"
SOURCE = "core/src/test/java/org/apache/calcite/rel/rel2sql/RelToSqlConverterTest.java"
DEST = Path(__file__).resolve().parents[2] / "crates/sqlscope/tests/fixtures/calcite/trino.json"
JAVA_STRING = r'"(?:[^"\\]|\\.)*"'
TERM = rf"(?:{JAVA_STRING}|\w+)"
VALUE = re.compile(rf"\bString (\w+)\s*=\s*({TERM}(?:\s*\+\s*{TERM})*)\s*;")
ASSERTION = re.compile(r"\.with(Trino|Mysql|StarRocks)\(\)\s*\.ok\((\w+)\)")


def literals(body):
    values = {}
    for name, value in VALUE.findall(body):
        terms = re.findall(TERM, value)
        if all(t.startswith('"') or t in values for t in terms):
            values[name] = "".join(json.loads(t) if t.startswith('"') else values[t] for t in terms)
    return values


def reviewed_alternatives(test, calcite, supplemental):
    """Equivalent target spellings, and corrections to supplemental SQLGlot expectations."""
    out = {}
    singleton = " FROM (SELECT 0 AS `ZERO`) AS `t`"
    if test in {"testPiFunction", "testPiFunctionWithoutParentheses"}:
        out["mysql"] = calcite["mysql"] + singleton
    if test == "testMapValueConstructor":
        # The two literal map constructors pair the same keys and values.
        out["starrocks"] = "SELECT MAP('k1', 'v1', 'k2', 'v2')" + singleton
    if test == "testSelectQueryWithSingletonCube":
        for target, expected in calcite.items():
            out[target] = re.sub(r"(`product_class_id`|COUNT\(\*\)) IS NULL",
                                 r"CASE WHEN \1 IS NULL THEN 1 ELSE 0 END", expected)
    if test == "testOrderByOrdinalWithExpression":
        for target in ("mysql", "starrocks"):
            out[target] = ("SELECT `product_id`, COUNT(*) AS `c` FROM `foodmart`.`product` "
                           "GROUP BY `product_id` ORDER BY CASE WHEN COUNT(*) IS NULL THEN 1 ELSE 0 END, 2")
    if test == "testRollupWithGroupBy":
        out["mysql"] = supplemental["mysql"].replace("ROLLUP (`product_id`, `product_class_id`)",
                                                       "`product_id`, `product_class_id` WITH ROLLUP")
    if test in {"testCubeWithGroupBy", "testSelectApproxCountDistinct"}:
        # MySQL has neither GROUP BY CUBE nor the Trino APPROX_DISTINCT aggregate.
        out["mysql"] = None
    return out


def main():
    checkout = Path(sys.argv[1])
    text = subprocess.check_output(["git", "-C", checkout, "show", f"{REV}:{SOURCE}"], text=True)
    if sqlglot.__version__ != "30.33.0":
        raise ValueError("Expected SQLGlot 30.33.0 for supplemental target expectations")
    methods = list(re.finditer(r"\bvoid (test\w+)\([^)]*\)[^{]*\{", text))
    cases = []
    for index, method in enumerate(methods):
        stop = methods[index + 1].start() if index + 1 < len(methods) else len(text)
        body = text[method.end():stop]
        values = literals(body)
        for chain in body.split(";"):
            if ".withTrino()" not in chain:
                continue
            assertions = {dialect.lower(): values[var] for dialect, var in ASSERTION.findall(chain)}
            source = assertions.pop("trino")
            supplemental = {}
            for target in ("mysql", "starrocks"):
                if target not in assertions:
                    try:
                        supplemental[target] = sqlglot.transpile(source, read="trino", write=target,
                                                                 unsupported_level=ErrorLevel.RAISE)[0]
                    except UnsupportedError:
                        supplemental[target] = None
            cases.append(dict(test=method[1], input=source, calcite=assertions, sqlglot=supplemental,
                              reviewed=reviewed_alternatives(method[1], assertions, supplemental)))
    assert len(cases) == 32
    assert sum(len(c["calcite"]) for c in cases) == 25
    DEST.write_text(json.dumps(dict(revision=REV, cases=cases), indent=2, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
