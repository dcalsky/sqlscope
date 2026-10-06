"""Scope-aware SQL analysis and rewriting.

=====================  ==========================================================
Function               Purpose
=====================  ==========================================================
apply_row_filter       Filter every in-scope table by a predicate.
inject_ctes            Prepend CTE definitions to a query's root ``WITH``.
rewrite_tables         Replace table references with derived tables.
column_origins         Source columns whose values reach the result (lineage).
output_columns         Names of the columns a statement outputs.
referenced_columns     Columns referenced anywhere, per table.
column_usages          Column references with the clause they appear in.
=====================  ==========================================================

Every function accepts ``dialect`` (default ``"trino"``). Functions release the
GIL while they run and are safe to call from several threads.

The SQL engine is the sqlscope FFI shared library (``libsqlscope_ffi.so``,
``libsqlscope_ffi.dylib`` or ``sqlscope_ffi.dll``) published with each sqlscope
release. It is loaded on first use; see :func:`load` for how it is located.
This package neither bundles nor downloads it.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple, Union

from ._library import (
    LIBRARY_PATH_ENV,
    Error,
    InternalError,
    InvalidArgumentError,
    LibraryNotFoundError,
    ParseError,
    UnsupportedError,
    call,
    library_file_name,
    library_version,
    load,
)

__version__ = "0.3.0"

__all__ = [
    "Clause",
    "ColumnUsage",
    "CteDef",
    "Error",
    "InternalError",
    "InvalidArgumentError",
    "LIBRARY_PATH_ENV",
    "LibraryNotFoundError",
    "ParseError",
    "Schema",
    "TableRef",
    "TableRewrite",
    "UnionRewrite",
    "UnsupportedError",
    "apply_row_filter",
    "column_origins",
    "column_usages",
    "inject_ctes",
    "library_file_name",
    "library_version",
    "load",
    "output_columns",
    "referenced_columns",
    "rewrite_tables",
]

Schema = Mapping[str, Sequence[str]]
"""Table name (bare, ``schema.table`` or ``catalog.schema.table``) -> ordered column names."""


@dataclass(frozen=True)
class CteDef:
    """A common table expression ``name AS (query)``.

    ``name`` is an identifier value, not SQL text; it is quoted as needed.
    """

    name: str
    query: str


@dataclass(frozen=True)
class TableRef:
    """A table name, optionally schema- and catalog-qualified."""

    table: str
    schema: Optional[str] = None
    catalog: Optional[str] = None


@dataclass(frozen=True)
class UnionRewrite:
    """A derived table backed by ``UNION DISTINCT`` over ``branches``."""

    table_alias: str
    columns: Sequence[str]
    branches: Sequence[TableRef]


@dataclass(frozen=True)
class TableRewrite:
    """Replaces references to ``match_key`` (``schema.table``).

    Set exactly one of ``inline`` (``(SELECT * FROM inline)``) or ``union``.
    """

    match_key: str
    inline: Optional[TableRef] = None
    union: Optional[UnionRewrite] = None


class Clause(str, Enum):
    """The SQL clause that contains a column reference."""

    SELECT = "SELECT"
    FROM = "FROM"
    JOIN_ON = "JOIN_ON"
    JOIN_USING = "JOIN_USING"
    WHERE = "WHERE"
    GROUP_BY = "GROUP_BY"
    HAVING = "HAVING"
    QUALIFY = "QUALIFY"
    WINDOW = "WINDOW"
    ORDER_BY = "ORDER_BY"
    SORT_BY = "SORT_BY"
    DISTRIBUTE_BY = "DISTRIBUTE_BY"
    CLUSTER_BY = "CLUSTER_BY"
    CONNECT_BY = "CONNECT_BY"
    LATERAL_VIEW = "LATERAL_VIEW"
    UPDATE_SET_TARGET = "UPDATE_SET_TARGET"
    UPDATE_SET_VALUE = "UPDATE_SET_VALUE"
    MERGE_ON = "MERGE_ON"
    MERGE_WHEN = "MERGE_WHEN"

    def __str__(self) -> str:
        return self.value


@dataclass(frozen=True, order=True)
class ColumnUsage:
    """One distinct use of a root table column in a clause."""

    table: str
    column: str
    clause: Clause = field(compare=True)


def _schema(schema: Optional[Schema]) -> Optional[Dict[str, List[str]]]:
    if schema is None:
        return None
    return {str(table): [str(column) for column in columns] for table, columns in schema.items()}


def _list(values: Optional[Iterable[str]]) -> Optional[List[str]]:
    if values is None:
        return None
    if isinstance(values, str):
        return [values]
    return list(values)


def _run(operation: str, sql: str, *, dialect: Optional[str], **fields: Any) -> Any:
    """Calls ``operation``; ``fields`` holds request fields and options, ``None`` meaning unset."""
    options = {"dialect": dialect}
    for name in ("schema", "tableNames", "tablePatterns", "defaultDb", "stripCatalogs"):
        options[name] = fields.pop(name, None)
    request = {"sql": sql, **fields, "options": {k: v for k, v in options.items() if v is not None}}
    return call(operation, request)


def apply_row_filter(
    sql: str,
    predicate: str,
    *,
    dialect: Optional[str] = None,
    table_names: Optional[Iterable[str]] = None,
    table_patterns: Optional[Iterable[str]] = None,
    default_db: Optional[str] = None,
) -> str:
    """Wrap every in-scope table of a query in a derived table filtered by ``predicate``.

    ``SELECT * FROM a`` becomes ``SELECT * FROM (SELECT * FROM a WHERE <predicate>) AS a``.
    By default every physical table is filtered; ``table_names`` (bare,
    ``schema.table`` or ``catalog.schema.table``), ``table_patterns`` (regular
    expressions) and ``default_db`` restrict the scope. CTE references, table
    functions and ``DUAL`` are never wrapped. ``predicate`` is parsed as one
    boolean expression; bind or escape its values before calling. Returns the
    input unchanged when no table is in scope.
    """
    result: str = _run(
        "apply_row_filter",
        sql,
        predicate=predicate,
        dialect=dialect,
        tableNames=_list(table_names),
        tablePatterns=_list(table_patterns),
        defaultDb=default_db,
    )
    return result


def inject_ctes(
    sql: str,
    ctes: Iterable[Union[CteDef, Tuple[str, str]]],
    *,
    dialect: Optional[str] = None,
) -> str:
    """Add CTE definitions to the root ``WITH`` of a query, before existing CTEs.

    Definitions keep their order, so each may use earlier ones. A name that
    repeats another definition or an existing root CTE is rejected.
    """
    defs = [
        {"name": cte.name, "query": cte.query} if isinstance(cte, CteDef) else {"name": cte[0], "query": cte[1]}
        for cte in ctes
    ]
    result: str = _run("inject_ctes", sql, ctes=defs, dialect=dialect)
    return result


def _table(ref: TableRef) -> Dict[str, Optional[str]]:
    return {"table": ref.table, "schema": ref.schema, "catalog": ref.catalog}


def rewrite_tables(
    sql: str,
    rewrites: Iterable[TableRewrite],
    *,
    dialect: Optional[str] = None,
    strip_catalogs: Optional[Iterable[str]] = None,
) -> str:
    """Replace physical table references according to ``rewrites``.

    Matched references become derived tables that keep their alias (or bare
    name); qualified column references are rebound onto it. Catalogs listed in
    ``strip_catalogs`` are transparent while matching. Returns the input
    unchanged when nothing matches.
    """
    plan = [
        {
            "matchKey": rewrite.match_key,
            "inline": _table(rewrite.inline) if rewrite.inline is not None else None,
            "union": {
                "tableAlias": rewrite.union.table_alias,
                "columns": list(rewrite.union.columns),
                "branches": [_table(branch) for branch in rewrite.union.branches],
            }
            if rewrite.union is not None
            else None,
        }
        for rewrite in rewrites
    ]
    result: str = _run("rewrite_tables", sql, rewrites=plan, dialect=dialect, stripCatalogs=_list(strip_catalogs))
    return result


def column_origins(
    sql: str, *, dialect: Optional[str] = None, schema: Optional[Schema] = None
) -> Dict[str, List[str]]:
    """Source columns whose values flow into the result, keyed by root table.

    Filter-only positions (WHERE, JOIN ON, GROUP BY, HAVING, ORDER BY) and the
    right side of INTERSECT / EXCEPT are excluded. Every table read is present,
    possibly with an empty list.
    """
    result: Dict[str, List[str]] = _run("column_origins", sql, dialect=dialect, schema=_schema(schema))
    return result


def output_columns(
    sql: str, *, dialect: Optional[str] = None, schema: Optional[Schema] = None
) -> Optional[List[str]]:
    """Names of the columns a statement outputs, in order.

    Unaliased expressions are named ``_col{i}``; ``*`` expands from ``schema``
    or stays ``"*"``. Returns ``None`` for statements without columns.
    """
    result: Optional[List[str]] = _run("output_columns", sql, dialect=dialect, schema=_schema(schema))
    return result


def referenced_columns(
    sql: str, *, dialect: Optional[str] = None, schema: Optional[Schema] = None
) -> Dict[str, List[str]]:
    """Columns referenced anywhere in a statement (including filters), keyed by root table."""
    result: Dict[str, List[str]] = _run("referenced_columns", sql, dialect=dialect, schema=_schema(schema))
    return result


def column_usages(
    sql: str, *, dialect: Optional[str] = None, schema: Optional[Schema] = None
) -> List[ColumnUsage]:
    """Every distinct ``(table, column, clause)`` use in a statement, sorted."""
    return [
        ColumnUsage(usage["table"], usage["column"], Clause(usage["clause"]))
        for usage in _run("column_usages", sql, dialect=dialect, schema=_schema(schema))
    ]
