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
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Dict, Iterable, List, Mapping, Optional, Sequence, Tuple, Union

from . import _native
from ._native import (
    Error,
    InternalError,
    InvalidArgumentError,
    ParseError,
    UnsupportedError,
)

__version__: str = _native.__version__

__all__ = [
    "Clause",
    "ColumnUsage",
    "CteDef",
    "Error",
    "InternalError",
    "InvalidArgumentError",
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
    return _native.apply_row_filter(
        sql,
        predicate,
        dialect=dialect,
        table_names=_list(table_names),
        table_patterns=_list(table_patterns),
        default_db=default_db,
    )


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
    pairs = [(cte.name, cte.query) if isinstance(cte, CteDef) else (cte[0], cte[1]) for cte in ctes]
    return _native.inject_ctes(sql, pairs, dialect=dialect)


def _table(ref: TableRef) -> Tuple[str, Optional[str], Optional[str]]:
    return (ref.table, ref.schema, ref.catalog)


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
        (
            rewrite.match_key,
            _table(rewrite.inline) if rewrite.inline is not None else None,
            (
                rewrite.union.table_alias,
                list(rewrite.union.columns),
                [_table(branch) for branch in rewrite.union.branches],
            )
            if rewrite.union is not None
            else None,
        )
        for rewrite in rewrites
    ]
    return _native.rewrite_tables(sql, plan, dialect=dialect, strip_catalogs=_list(strip_catalogs))


def column_origins(
    sql: str, *, dialect: Optional[str] = None, schema: Optional[Schema] = None
) -> Dict[str, List[str]]:
    """Source columns whose values flow into the result, keyed by root table.

    Filter-only positions (WHERE, JOIN ON, GROUP BY, HAVING, ORDER BY) and the
    right side of INTERSECT / EXCEPT are excluded. Every table read is present,
    possibly with an empty list.
    """
    return _native.column_origins(sql, dialect=dialect, schema=_schema(schema))


def output_columns(
    sql: str, *, dialect: Optional[str] = None, schema: Optional[Schema] = None
) -> Optional[List[str]]:
    """Names of the columns a statement outputs, in order.

    Unaliased expressions are named ``_col{i}``; ``*`` expands from ``schema``
    or stays ``"*"``. Returns ``None`` for statements without columns.
    """
    return _native.output_columns(sql, dialect=dialect, schema=_schema(schema))


def referenced_columns(
    sql: str, *, dialect: Optional[str] = None, schema: Optional[Schema] = None
) -> Dict[str, List[str]]:
    """Columns referenced anywhere in a statement (including filters), keyed by root table."""
    return _native.referenced_columns(sql, dialect=dialect, schema=_schema(schema))


def column_usages(
    sql: str, *, dialect: Optional[str] = None, schema: Optional[Schema] = None
) -> List[ColumnUsage]:
    """Every distinct ``(table, column, clause)`` use in a statement, sorted."""
    return [
        ColumnUsage(table, column, Clause(clause))
        for table, column, clause in _native.column_usages(sql, dialect=dialect, schema=_schema(schema))
    ]
