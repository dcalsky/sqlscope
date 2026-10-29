//! Shared helpers over the polyglot typed AST.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::mem::{discriminant, Discriminant};

use polyglot_sql::expressions::{Column, DotAccess, Identifier, TableRef, With};
use polyglot_sql::{traversal::transform_all, ComplexityGuardOptions, Expression, ExpressionWalk, ParseOptions};

use crate::error::{Error, Result};
use crate::options::Dialect;

/// Upper bound on any SQL text accepted or produced, independent of the
/// parser's own complexity guards.
pub(crate) const MAX_INPUT_BYTES: usize = 1 << 20;

pub(crate) fn guard_input(sql: &str) -> Result<()> {
    if sql.len() > MAX_INPUT_BYTES {
        return Err(Error::unsupported(format!(
            "input too large ({} bytes, limit {MAX_INPUT_BYTES})",
            sql.len()
        )));
    }
    Ok(())
}

/// Maps a polyglot parse failure to [`Error`]. Complexity-guard rejections
/// (`E_GUARD_*`) are bounded-input failures and classify as unsupported.
fn classify_parse_error(error: polyglot_sql::Error) -> Error {
    let message = error.to_string();
    if message.contains("E_GUARD_") {
        Error::unsupported(message)
    } else {
        Error::parse(message)
    }
}

/// Parser recursion limit. Set explicitly so every build target (native and
/// WebAssembly, whose polyglot defaults differ) accepts the same inputs.
pub(crate) const MAX_PARSER_DEPTH: usize = 256;

fn parse_options() -> ParseOptions {
    ParseOptions {
        complexity_guard: Some(ComplexityGuardOptions {
            max_parser_depth: Some(MAX_PARSER_DEPTH),
            ..ComplexityGuardOptions::default()
        }),
    }
}

/// MySQL-compatible servers execute the body of `/*! ... */` (and MariaDB's
/// `/*M! ... */`) as SQL, depending on the server version, while the parser
/// reads a comment. Analysis would miss the hidden code, so such input is
/// rejected. The text is searched as a whole, string literals included, so
/// no quoting mode can hide one.
fn guard_executable_comments(sql: &str, dialect: Dialect) -> Result<()> {
    use polyglot_sql::DialectType::{Doris, MySQL, SingleStore, StarRocks, TiDB};
    if !matches!(dialect.polyglot(), MySQL | SingleStore | TiDB | Doris | StarRocks) {
        return Ok(());
    }
    let executable = sql.match_indices("/*").any(|(at, _)| {
        let rest = &sql.as_bytes()[at + 2..];
        rest.first() == Some(&b'!') || (matches!(rest.first(), Some(b'M' | b'm')) && rest.get(1) == Some(&b'!'))
    });
    if executable {
        return Err(Error::unsupported("executable comments (/*! ... */) are not supported"));
    }
    Ok(())
}

/// The parser nests block comments for MySQL, BigQuery and Hive, whose
/// engines end a comment at the first `*/`: in `/* /* */ , secret -- */` the
/// engine reads `, secret` as code while the parser reads one comment. Input
/// that tokenizes differently without nesting is rejected.
fn guard_nested_comments(sql: &str, dialect: Dialect) -> Result<()> {
    use polyglot_sql::dialects::{BigQueryDialect, DialectImpl, HiveDialect, MySQLDialect};
    use polyglot_sql::Tokenizer;
    // Nesting needs a comment opened inside another one.
    if sql.matches("/*").nth(1).is_none() {
        return Ok(());
    }
    let nested = match dialect.polyglot() {
        polyglot_sql::DialectType::MySQL => MySQLDialect.tokenizer_config(),
        polyglot_sql::DialectType::BigQuery => BigQueryDialect.tokenizer_config(),
        polyglot_sql::DialectType::Hive => HiveDialect.tokenizer_config(),
        _ => return Ok(()),
    };
    if !nested.nested_comments {
        return Ok(());
    }
    let mut flat = nested.clone();
    flat.nested_comments = false;
    let key = |token: &polyglot_sql::Token| (token.token_type, token.span.start, token.span.end);
    let same = match (Tokenizer::new(nested).tokenize(sql), Tokenizer::new(flat).tokenize(sql)) {
        (Ok(nested), Ok(flat)) => nested.iter().map(key).eq(flat.iter().map(key)),
        (Err(_), _) => true, // the parser reports the error
        (Ok(_), Err(_)) => false,
    };
    if !same {
        return Err(Error::unsupported("nested block comments are not supported"));
    }
    Ok(())
}

/// Table functions that read catalog tables named by their arguments, or run
/// SQL text: ClickHouse `remote`, `cluster`, `merge`, ..., DuckDB
/// `query_table` / `query`, BigQuery `EXTERNAL_QUERY`, PostgreSQL `dblink`,
/// T-SQL `OPENQUERY` / `OPENROWSET` / `OPENDATASOURCE`.
const TABLE_READING_FUNCTIONS: &[&str] = &[
    "remote",
    "remotesecure",
    "cluster",
    "clusterallreplicas",
    "merge",
    "loop",
    "query_table",
    "query",
    "external_query",
    "dblink",
    "openquery",
    "openrowset",
    "opendatasource",
];

/// Rejects table references no rewrite can scope and no analysis can name:
/// `IDENTIFIER(...)` (a table or column named by an expression), Snowflake
/// table literals (`TABLE('t')`, `TABLE($t)`), relations produced by
/// [`TABLE_READING_FUNCTIONS`] (also inside `TABLE(...)`, as in Trino's
/// `TABLE(system.query(...))`), ClickHouse `joinGet('t', ...)`, and a bare
/// name on the right of IN (`x IN t`), which ClickHouse reads as a table.
fn guard_hidden_tables(statement: &Expression) -> Result<()> {
    fn call_name(expression: &Expression) -> Option<String> {
        match expression {
            Expression::Function(function) => Some(function.name.to_lowercase()),
            Expression::MethodCall(call) => Some(call.method.name.to_lowercase()),
            _ => None,
        }
    }
    fn reads_tables(name: &str) -> bool {
        TABLE_READING_FUNCTIONS.contains(&name.rsplit('.').next().unwrap_or(name))
    }
    fn hidden_relation(entry: &Expression) -> Option<String> {
        match entry {
            Expression::Alias(alias) => hidden_relation(&alias.this),
            Expression::Paren(paren) => hidden_relation(&paren.this),
            Expression::Function(function) => {
                let name = function.name.to_lowercase();
                if name == "table" {
                    // TABLE(<name or string>) is a table literal; TABLE(f(...))
                    // runs a table function.
                    return match function.args.as_slice() {
                        [argument] => match call_name(argument) {
                            Some(inner) => reads_tables(&inner).then_some(inner),
                            None => Some(name),
                        },
                        _ => None,
                    };
                }
                reads_tables(&name).then_some(name)
            }
            _ => None,
        }
    }
    for node in statement.dfs() {
        match node {
            Expression::Function(function) if function.name.eq_ignore_ascii_case("identifier") => {
                return Err(Error::unsupported("IDENTIFIER(...) references are not supported"));
            }
            Expression::Function(function) if function.name.to_lowercase().starts_with("joinget") => {
                return Err(Error::unsupported(format!(
                    "{}(...) reads a table that cannot be scoped",
                    function.name
                )));
            }
            Expression::In(predicate)
                if predicate.is_field
                    && predicate
                        .expressions
                        .iter()
                        .any(|item| matches!(item, Expression::Column(_) | Expression::Dot(_))) =>
            {
                return Err(Error::unsupported(
                    "IN <table> is not supported; write IN (SELECT ...) instead",
                ));
            }
            _ => {}
        }
        let entries: Vec<&Expression> = match node {
            Expression::Select(select) => crate::rewrite::from_entries(select),
            Expression::Update(update) => update
                .from_clause
                .iter()
                .flat_map(|from| from.expressions.iter())
                .chain(
                    update
                        .table_joins
                        .iter()
                        .chain(&update.from_joins)
                        .map(|join| &join.this),
                )
                .collect(),
            Expression::Delete(delete) => delete.joins.iter().map(|join| &join.this).collect(),
            Expression::Merge(merge) => vec![&merge.this, &merge.using],
            _ => continue,
        };
        if let Some(name) = entries.into_iter().find_map(hidden_relation) {
            return Err(Error::unsupported(format!(
                "{name}(...) reads tables that cannot be scoped"
            )));
        }
    }
    Ok(())
}

/// T-SQL table hints, which may follow a table without `WITH` (`t (NOLOCK)`).
const TSQL_TABLE_HINTS: &[&str] = &[
    "nolock",
    "readuncommitted",
    "updlock",
    "repeatableread",
    "serializable",
    "readcommitted",
    "readcommittedlock",
    "tablock",
    "tablockx",
    "paglock",
    "rowlock",
    "nowait",
    "readpast",
    "xlock",
    "snapshot",
    "noexpand",
    "holdlock",
    "forcescan",
    "forceseek",
    "keepidentity",
    "keepdefaults",
    "ignore_constraints",
    "ignore_triggers",
    "index",
    "spatial_window_max_cells",
];

/// T-SQL accepts table hints without `WITH`, which the parser reads as
/// something else: `FROM t (NOLOCK)` as a call of a function `t` (hiding the
/// table), `FROM t AS x (NOLOCK)` as column aliases (renaming a column once
/// the table is wrapped). Both are rejected; `WITH (NOLOCK)` parses.
fn guard_legacy_table_hints(statement: &Expression, dialect: Dialect) -> Result<()> {
    fn hint(argument: &Expression) -> bool {
        let name = match argument {
            Expression::Column(column) if column.table.is_none() => &column.name.name,
            Expression::Function(function) => &function.name,
            Expression::Eq(eq) => match &eq.left {
                Expression::Column(column) if column.table.is_none() => &column.name.name,
                _ => return false,
            },
            _ => return false,
        };
        TSQL_TABLE_HINTS.contains(&name.to_lowercase().as_str())
    }
    fn hinted(entry: &Expression) -> bool {
        match entry {
            Expression::Alias(alias) => hinted(&alias.this),
            Expression::Function(function) => !function.args.is_empty() && function.args.iter().all(hint),
            _ => false,
        }
    }
    if dialect != Dialect::TSQL {
        return Ok(());
    }
    let rejected = || Error::unsupported("table hints without WITH are not supported; write WITH (NOLOCK)");
    for node in statement.dfs() {
        let entries: Vec<&Expression> = match node {
            Expression::Table(table) if !table.column_aliases.is_empty() => return Err(rejected()),
            Expression::Select(select) => crate::rewrite::from_entries(select),
            Expression::Update(update) => update
                .from_clause
                .iter()
                .flat_map(|from| from.expressions.iter())
                .chain(
                    update
                        .table_joins
                        .iter()
                        .chain(&update.from_joins)
                        .map(|join| &join.this),
                )
                .collect(),
            Expression::Delete(delete) => delete.joins.iter().map(|join| &join.this).collect(),
            _ => continue,
        };
        if entries.into_iter().any(hinted) {
            return Err(rejected());
        }
    }
    Ok(())
}

/// The parser keeps `FOR SYSTEM_TIME ...` (T-SQL temporal tables) as raw
/// text, copied token by token up to a few stop words: `FROM c TO d` runs on
/// through `UNION ALL SELECT ...` or `JOIN t ...`, which the rewrites then
/// re-emit unseen. Only the tokens of a time value are accepted.
fn guard_system_time(statement: &Expression, dialect: Dialect) -> Result<()> {
    use polyglot_sql::TokenType as T;
    for node in statement.dfs() {
        let Expression::Table(table) = node else {
            continue;
        };
        let Some(text) = table.system_time.as_deref() else {
            continue;
        };
        let tokens = polyglot_sql::Dialect::get(dialect.polyglot())
            .tokenize(text)
            .map_err(|_| Error::unsupported("FOR SYSTEM_TIME clause cannot be read"))?;
        let froms = tokens.iter().filter(|token| token.token_type == T::From).count();
        let value = tokens.iter().all(|token| {
            matches!(
                token.token_type,
                T::For
                    | T::From
                    | T::To
                    | T::As
                    | T::Of
                    | T::Between
                    | T::And
                    | T::In
                    | T::All
                    | T::Cast
                    | T::Var
                    | T::Identifier
                    | T::QuotedIdentifier
                    | T::String
                    | T::NationalString
                    | T::Number
                    | T::LParen
                    | T::RParen
                    | T::Comma
                    | T::Dot
                    | T::Plus
                    | T::Dash
                    | T::Star
                    | T::Slash
            )
        });
        if !value || froms > 1 {
            return Err(Error::unsupported(format!(
                "FOR SYSTEM_TIME clause {text:?} is not supported"
            )));
        }
    }
    Ok(())
}

/// Runs a polyglot call, turning a panic into an error where panics unwind
/// (native builds; WebAssembly aborts). `what` names the component.
fn without_panics<R>(what: &str, f: impl FnOnce() -> R) -> Result<R> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|message| message.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        Error::internal(format!("{what} panicked: {message}"))
    })
}

/// The parser on its own: the outer result reports a parser panic, the inner
/// one the parse.
fn parse_raw(sql: &str, dialect: Dialect) -> Result<Result<Vec<Expression>>> {
    without_panics("SQL parser", || {
        polyglot_sql::parse_with_options(sql, dialect.polyglot(), &parse_options()).map_err(classify_parse_error)
    })
}

/// Rejects input whose parentheses do not balance, which never parses. The
/// parser can panic on an unclosed one (T-SQL `SELECT IF(IF(`), and a panic
/// aborts the WebAssembly module, so it is caught before parsing; the
/// ClickHouse parser skips what follows a stray `)` instead of failing.
fn guard_unbalanced_parentheses(sql: &str, dialect: Dialect) -> Result<()> {
    if !sql.contains('(') && !sql.contains(')') {
        return Ok(());
    }
    let Ok(tokens) = polyglot_sql::Dialect::get(dialect.polyglot()).tokenize(sql) else {
        return Ok(()); // the parser reports the error
    };
    let mut depth = 0isize;
    for token in &tokens {
        match token.token_type {
            polyglot_sql::TokenType::LParen => depth += 1,
            polyglot_sql::TokenType::RParen => depth -= 1,
            _ => continue,
        }
        if depth < 0 {
            return Err(Error::parse("unmatched closing parenthesis"));
        }
    }
    if depth > 0 {
        return Err(Error::parse("unclosed parenthesis"));
    }
    Ok(())
}

/// The ClickHouse parser skips whatever it cannot read up to the end of the
/// statement instead of failing: `FROM a LOCAL JOIN t USING (id)` parses as
/// `FROM a`, so the join and table `t` vanish. Skipping removes a suffix, so
/// a word after the last identifier the AST holds (by source position) means
/// input was lost. Function names, built-in types, words other dialects read
/// as keywords, `{param: Type}` query parameters and the trailing SETTINGS /
/// FORMAT clauses (read by the parser, but not kept) are not counted; the
/// column definitions of a CREATE TABLE without a query are not checked.
fn guard_skipped_input(sql: &str, statement: &Expression, dialect: Dialect) -> Result<()> {
    use polyglot_sql::TokenType;
    if dialect != Dialect::CLICKHOUSE {
        return Ok(());
    }
    let Ok(tokens) = polyglot_sql::Dialect::get(dialect.polyglot()).tokenize(sql) else {
        return Ok(());
    };
    if matches!(statement, Expression::CreateTable(table) if table.as_select.is_none()) {
        // Column definitions are not checked, but a query must not vanish
        // (`CREATE TABLE t (a, b) AS SELECT ...` parses without it).
        let mut depth = 0isize;
        let top_level_select = tokens.iter().any(|token| {
            match token.token_type {
                TokenType::LParen => depth += 1,
                TokenType::RParen => depth -= 1,
                _ => {}
            }
            depth == 0 && token.token_type == TokenType::Select
        });
        if top_level_select {
            return Err(Error::unsupported("the parser skipped the query of CREATE TABLE"));
        }
        return Ok(());
    }
    fn reach(value: &serde_json::Value, end: &mut usize) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(span_end) = object
                    .get("span")
                    .and_then(|span| span.get("end"))
                    .and_then(|end| end.as_u64())
                {
                    *end = (*end).max(span_end as usize);
                }
                object.values().for_each(|value| reach(value, end));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|value| reach(value, end)),
            _ => {}
        }
    }
    let mut end = 0;
    let tree = with_large_stack(|| serde_json::to_value(statement))
        .map_err(|error| Error::internal(format!("AST inspection failed: {error}")))?;
    with_large_stack(|| reach(&tree, &mut end));

    let mut last = tokens.len();
    while last > 0 && tokens[last - 1].token_type == TokenType::Semicolon {
        last -= 1;
    }
    if let Some(tail) = tokens[..last]
        .iter()
        .rposition(|token| matches!(token.token_type, TokenType::Settings | TokenType::Format))
    {
        last = tail;
    }
    // Skipping starts where the statement ended, outside any parentheses.
    let mut depth = 0isize;
    let mut subcolumn = false;
    for (index, token) in tokens[..last].iter().enumerate() {
        // `json.a.:JSON.b.:Int64` names subcolumn types, kept without positions.
        let previous = index
            .checked_sub(1)
            .map(|at| tokens[at].text.as_str())
            .unwrap_or_default();
        subcolumn = previous.ends_with(':')
            || (subcolumn && (previous == "." || token.text == "." || token.text.ends_with(':')));
        match token.token_type {
            TokenType::LParen | TokenType::LBracket | TokenType::LBrace => depth += 1,
            TokenType::RParen | TokenType::RBracket | TokenType::RBrace => depth -= 1,
            _ => {}
        }
        let word = matches!(
            token.token_type,
            TokenType::Var | TokenType::Identifier | TokenType::QuotedIdentifier
        );
        let call = tokens
            .get(index + 1)
            .is_some_and(|next| next.token_type == TokenType::LParen);
        if token.span.start < end
            || depth != 0
            || !word
            || call
            || subcolumn
            || CLICKHOUSE_UNKEPT_WORDS
                .iter()
                .any(|kept| token.text.eq_ignore_ascii_case(kept))
            || is_type_or_keyword(&token.text)
        {
            continue;
        }
        return Err(Error::unsupported(format!(
            "the parser skipped part of the statement, starting near {:?}",
            token.text
        )));
    }
    Ok(())
}

/// Words the ClickHouse parser reads without keeping a source position:
/// `WITH TOTALS` and interval units.
const CLICKHOUSE_UNKEPT_WORDS: &[&str] = &[
    "totals",
    "nanosecond",
    "microsecond",
    "millisecond",
    "second",
    "minute",
    "hour",
    "day",
    "week",
    "month",
    "quarter",
    "year",
];

/// Whether `word` names a built-in data type, or is read as a keyword by
/// some dialect's tokenizer.
fn is_type_or_keyword(word: &str) -> bool {
    use polyglot_sql::DialectType::*;
    if matches!(
        polyglot_sql::parse_data_type(word, ClickHouse),
        Ok(data_type) if !matches!(data_type, polyglot_sql::expressions::DataType::Custom { .. })
    ) {
        return true;
    }
    [
        ClickHouse, PostgreSQL, MySQL, BigQuery, Snowflake, Spark, DuckDB, TSQL, Oracle, Hive, SQLite, Trino,
    ]
    .into_iter()
    .any(|dialect| {
        polyglot_sql::Dialect::get(dialect)
            .tokenize(word)
            .map_or(true, |tokens| {
                tokens.len() != 1 || tokens[0].token_type != polyglot_sql::TokenType::Var
            })
    })
}

/// Clause keywords the parser reads as an implicit table alias in these
/// dialects: ClickHouse `a GLOBAL JOIN b` and `a PASTE JOIN b`, Oracle
/// `a PARTITION (p)` (which also turns `p` into a column alias), Doris
/// `a PARTITIONS (p)`. The result means something else than the input.
fn guard_keyword_aliases(statement: &Expression, dialect: Dialect) -> Result<()> {
    use polyglot_sql::DialectType::{ClickHouse, Doris, Oracle};
    let keywords: &[&str] = match dialect.polyglot() {
        ClickHouse => &["global", "paste"],
        Oracle => &["partition", "subpartition"],
        Doris => &["partitions"],
        _ => return Ok(()),
    };
    for node in statement.dfs() {
        if let Expression::Table(table) = node {
            if let Some(alias) = table.alias.as_ref().filter(|alias| !alias.quoted) {
                if keywords.iter().any(|keyword| alias.name.eq_ignore_ascii_case(keyword)) {
                    return Err(Error::unsupported(format!(
                        "{} after table {:?} is not supported here",
                        alias.name.to_uppercase(),
                        table.name.name
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Parses `sql` into its statements.
pub(crate) fn parse(sql: &str, dialect: Dialect) -> Result<Vec<Expression>> {
    guard_input(sql)?;
    guard_unbalanced_parentheses(sql, dialect)?;
    guard_executable_comments(sql, dialect)?;
    guard_nested_comments(sql, dialect)?;
    let statements: Vec<Expression> = parse_raw(sql, dialect)??
        .into_iter()
        .map(fold_table_aliases)
        .collect::<Result<_>>()?;
    statements.iter().try_for_each(guard_hidden_tables)?;
    for statement in &statements {
        guard_keyword_aliases(statement, dialect)?;
        guard_legacy_table_hints(statement, dialect)?;
        guard_system_time(statement, dialect)?;
    }
    if let [statement] = statements.as_slice() {
        guard_skipped_input(sql, statement, dialect)?;
    }
    if dialect != Dialect::BIGQUERY {
        return Ok(statements);
    }
    statements.into_iter().map(split_bigquery_paths).collect()
}

/// Brings parser output to the shapes rewrites and analysis read. The parser
/// wraps some aliased FROM and JOIN tables in an alias node (Snowflake `ASOF
/// JOIN y LIMIT MATCH_CONDITION (...)`), which become plain aliased tables,
/// and keeps some paths as one unquoted column name (Redshift `FROM t AS c,
/// c.arr AS o AT i`), which is split at its dots.
fn fold_table_aliases(statement: Expression) -> Result<Expression> {
    fn fold(entry: &mut Expression) {
        let Expression::Alias(alias) = entry else {
            return;
        };
        let Expression::Table(table) = &mut alias.this else {
            return;
        };
        if table.alias.is_some() {
            return;
        }
        table.alias = Some(alias.alias.clone());
        table.alias_explicit_as = alias.alias_explicit_as;
        table.column_aliases = std::mem::take(&mut alias.column_aliases);
        *entry = std::mem::replace(&mut alias.this, Expression::Null(polyglot_sql::expressions::Null));
    }
    with_large_stack(|| {
        transform_all(statement, &|mut node| {
            match &mut node {
                Expression::Select(select) => select
                    .from
                    .iter_mut()
                    .flat_map(|from| from.expressions.iter_mut())
                    .chain(select.joins.iter_mut().map(|join| &mut join.this))
                    .for_each(fold),
                Expression::Column(column) if column.table.is_none() && !column.name.quoted => {
                    let parts: Vec<&str> = column.name.name.split('.').collect();
                    if parts.len() > 1 && parts.iter().all(|part| !part.is_empty()) {
                        let mut parts: Vec<Identifier> = parts.into_iter().map(Identifier::new).collect();
                        let field = parts.pop().expect("a dotted name has two parts");
                        return Ok(match parts.len() {
                            1 => Expression::Column(Box::new(Column {
                                name: field,
                                table: parts.pop(),
                                join_mark: false,
                                trailing_comments: Vec::new(),
                                span: None,
                                inferred_type: None,
                            })),
                            _ => dot_chain(parts, field),
                        });
                    }
                }
                _ => {}
            }
            Ok(node)
        })
    })
    .map_err(|error| Error::internal(format!("AST transform failed: {error}")))
}

/// BigQuery reads a quoted table name containing dots as a path:
/// `` `proj.ds.orders` `` is `proj`.`ds`.`orders`. The parser keeps it as one
/// identifier, which would hide the dataset and project from name matching,
/// so every table reference, and every column or star qualifier written the
/// same way, is split into its parts.
fn split_bigquery_paths(statement: Expression) -> Result<Expression> {
    with_large_stack(|| {
        transform_all(statement, &|mut node| {
            match &mut node {
                Expression::Table(table) => split_bigquery_path(table),
                Expression::Update(update) => {
                    split_bigquery_path(&mut update.table);
                    update.extra_tables.iter_mut().for_each(split_bigquery_path);
                }
                Expression::Delete(delete) => {
                    split_bigquery_path(&mut delete.table);
                    delete.using.iter_mut().for_each(split_bigquery_path);
                    delete.tables.iter_mut().for_each(split_bigquery_path);
                }
                Expression::Insert(insert) => split_bigquery_path(&mut insert.table),
                Expression::Column(column) => {
                    if let Some(chain) = column.table.as_ref().and_then(dotted_parts) {
                        return Ok(dot_chain(chain, column.name.clone()));
                    }
                }
                Expression::Star(star) => {
                    // A star qualifier is one identifier whose unquoted dots
                    // separate the parts.
                    if let Some(table) = star.table.as_mut().filter(|t| t.quoted && t.name.contains('.')) {
                        table.quoted = false;
                    }
                }
                _ => {}
            }
            Ok(node)
        })
    })
    .map_err(|error| Error::internal(format!("AST transform failed: {error}")))
}

/// The parts of a dotted identifier: quoted (`` `proj.ds.orders` ``), or
/// unquoted, which the parser yields for a dashed path (`my-proj.orders`).
fn dotted_parts(identifier: &Identifier) -> Option<Vec<Identifier>> {
    if !identifier.name.contains('.') {
        return None;
    }
    let part = if identifier.quoted {
        Identifier::quoted
    } else {
        Identifier::new
    };
    let parts: Vec<Identifier> = identifier.name.split('.').map(part).collect();
    (parts.len() <= 3 && parts.iter().all(|part| !part.name.is_empty())).then_some(parts)
}

/// `a.b.c` + `field` as a column reference followed by field accesses, the
/// shape the parser gives an unquoted `a.b.c.field`.
fn dot_chain(mut qualifier: Vec<Identifier>, field: Identifier) -> Expression {
    let rest = qualifier.split_off(2.min(qualifier.len()));
    let mut parts = qualifier.into_iter();
    let first = parts.next().expect("a dotted name has two parts");
    let second = parts.next().expect("a dotted name has two parts");
    let mut expression = Expression::Column(Box::new(Column {
        name: second,
        table: Some(first),
        join_mark: false,
        trailing_comments: Vec::new(),
        span: None,
        inferred_type: None,
    }));
    for part in rest.into_iter().chain([field]) {
        expression = Expression::Dot(Box::new(DotAccess {
            this: expression,
            field: part,
            inferred_type: None,
        }));
    }
    expression
}

fn split_bigquery_path(table: &mut TableRef) {
    let written = [table.catalog.as_ref(), table.schema.as_ref(), Some(&table.name)];
    if !written
        .iter()
        .flatten()
        .any(|identifier| dotted_parts(identifier).is_some())
    {
        return;
    }
    let mut parts: Vec<Identifier> = Vec::with_capacity(3);
    for identifier in written.into_iter().flatten() {
        match dotted_parts(identifier) {
            Some(split) => parts.extend(split),
            None if !identifier.name.is_empty() => parts.push(identifier.clone()),
            None => {}
        }
    }
    if parts.len() > 3 {
        return; // not a project.dataset.table path; leave it as written
    }
    table.name = parts.pop().expect("a dotted name has two parts");
    table.schema = parts.pop();
    table.catalog = parts.pop();
}

/// Parses `sql`, which must contain exactly one statement.
pub(crate) fn parse_single(sql: &str, dialect: Dialect) -> Result<Expression> {
    let mut statements = parse(sql, dialect)?;
    if statements.len() != 1 {
        return Err(Error::unsupported(format!(
            "expected exactly one statement, got {}",
            statements.len()
        )));
    }
    Ok(statements.remove(0))
}

/// Parses `sql`, which must be a single SELECT or set operation that reads
/// only: a data-modifying CTE (`WITH d AS (DELETE ... RETURNING *)`) writes
/// and returns rows no rewrite can scope, so it is rejected.
pub(crate) fn parse_query(sql: &str, dialect: Dialect) -> Result<Expression> {
    let statement = parse_single(sql, dialect)?;
    if !is_query(&statement) {
        return Err(Error::unsupported(
            "only a SELECT or set operation (UNION / INTERSECT / EXCEPT) is supported",
        ));
    }
    let modifies = statement.dfs().any(|node| {
        matches!(
            node,
            Expression::Insert(_) | Expression::Update(_) | Expression::Delete(_) | Expression::Merge(_)
        )
    });
    if modifies {
        return Err(Error::unsupported(
            "data-modifying statements inside a query are not supported",
        ));
    }
    Ok(statement)
}

/// Renders `expression` as SQL.
pub(crate) fn generate(expression: &Expression, dialect: Dialect) -> Result<String> {
    // The generator recurses without growing the stack itself.
    without_panics("SQL generator", || {
        with_large_stack(|| polyglot_sql::generate(expression, dialect.polyglot()))
    })?
    .map_err(|error| Error::internal(format!("SQL generation failed: {error}")))
}

/// Renders a rewritten statement and proves that the output parses again, so
/// a rewrite either yields valid SQL or fails closed. `inputs` are the SQL
/// texts the statement was built from: when one of them does not survive
/// regeneration unchanged either, the input, not the rewrite, is at fault.
pub(crate) fn generate_checked(expression: &Expression, dialect: Dialect, inputs: &[&str]) -> Result<String> {
    guard_lossy_generation(expression, dialect)?;
    let sql = generate(expression, dialect)?;
    guard_input(&sql).map_err(|error| error.context("generated SQL"))?;
    if let Err(error) = parse_raw(&sql, dialect)? {
        if !inputs.iter().all(|input| round_trips(input, dialect)) {
            return Err(Error::unsupported(format!(
                "the input cannot be regenerated as SQL that parses: {error}"
            )));
        }
        return Err(Error::internal(format!(
            "rewritten SQL does not parse: {error}; SQL: {sql}"
        )));
    }
    Ok(sql)
}

/// Rejects what the generator parses but silently drops for `dialect`, so a
/// rewrite never changes what the rest of the statement means: the `(+)`
/// outer-join marker outside Oracle, Redshift and Exasol, `NTH_VALUE ...
/// FROM LAST` outside Snowflake, and BigQuery's `LAST_DAY(d, WEEK(<day>))`
/// and parameterized `NUMERIC(p, s)` cast targets.
fn guard_lossy_generation(statement: &Expression, dialect: Dialect) -> Result<()> {
    use polyglot_sql::expressions::{DataType, DateTimeField};
    use polyglot_sql::DialectType::{BigQuery, Exasol, Oracle, Redshift, Snowflake};
    fn parameterized_decimal(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                (map.get("data_type").and_then(|kind| kind.as_str()) == Some("decimal")
                    && map.get("precision").is_some_and(|precision| !precision.is_null()))
                    || map.values().any(parameterized_decimal)
            }
            serde_json::Value::Array(items) => items.iter().any(parameterized_decimal),
            _ => false,
        }
    }
    let parameterized =
        |data_type: &DataType| serde_json::to_value(data_type).is_ok_and(|value| parameterized_decimal(&value));
    let target = dialect.polyglot();
    for node in statement.dfs() {
        let lost = match node {
            Expression::Column(column) if column.join_mark && !matches!(target, Oracle | Redshift | Exasol) => {
                "the (+) outer join marker"
            }
            Expression::NthValue(function) if function.from_first == Some(false) && target != Snowflake => {
                "NTH_VALUE ... FROM LAST"
            }
            Expression::LastDay(function)
                if target == BigQuery
                    && matches!(&function.unit, Some(DateTimeField::WeekWithModifier(day)) if !day.eq_ignore_ascii_case("sunday")) =>
            {
                "LAST_DAY(..., WEEK(<day>))"
            }
            Expression::Cast(cast) | Expression::TryCast(cast) | Expression::SafeCast(cast)
                if target == BigQuery && parameterized(&cast.to) =>
            {
                "a NUMERIC(<precision>, <scale>) cast"
            }
            _ => continue,
        };
        return Err(Error::unsupported(format!("{lost} cannot be regenerated faithfully")));
    }
    Ok(())
}

/// Whether `sql` survives a parse and regeneration: the regenerated SQL
/// parses again and regenerates unchanged (the parser can accept it by
/// skipping part of it, as with ClickHouse `SAMPLE BERNOULLI 10 PERCENT`).
fn round_trips(sql: &str, dialect: Dialect) -> bool {
    parse(sql, dialect).is_ok_and(|statements| {
        statements.iter().all(|statement| {
            generate(statement, dialect).is_ok_and(|sql| {
                parse(&sql, dialect).is_ok_and(|again| {
                    again.len() == 1 && generate(&again[0], dialect).is_ok_and(|regenerated| regenerated == sql)
                })
            })
        })
    })
}

pub(crate) fn is_query(expression: &Expression) -> bool {
    matches!(
        expression,
        Expression::Select(_) | Expression::Union(_) | Expression::Intersect(_) | Expression::Except(_)
    )
}

/// The WITH clause attached to a query node.
pub(crate) fn query_with(expression: &Expression) -> Option<&With> {
    match expression {
        Expression::Select(select) => select.with.as_ref(),
        Expression::Union(op) => op.with.as_ref(),
        Expression::Intersect(op) => op.with.as_ref(),
        Expression::Except(op) => op.with.as_ref(),
        _ => None,
    }
}

pub(crate) fn query_with_mut(expression: &mut Expression) -> Option<&mut Option<With>> {
    match expression {
        Expression::Select(select) => Some(&mut select.with),
        Expression::Union(op) => Some(&mut op.with),
        Expression::Intersect(op) => Some(&mut op.with),
        Expression::Except(op) => Some(&mut op.with),
        _ => None,
    }
}

/// Finds the query a statement wraps: the statement itself for a SELECT or set
/// operation, or the query of CREATE VIEW / CREATE TABLE AS / INSERT ...
/// SELECT / EXPLAIN / CACHE TABLE and similar wrappers. `None` means the
/// statement contains no query (for example CREATE TABLE (...) or DROP).
pub(crate) fn inner_query(statement: &Expression) -> Option<&Expression> {
    if is_query(statement) {
        return Some(statement);
    }
    match statement {
        // INSERT can own a WITH clause outside its source query. Keep the
        // wrapper so the resolver does not lose that scope.
        Expression::Insert(insert) if insert.query.as_ref().and_then(inner_query).is_some() => Some(statement),
        Expression::Subquery(subquery) if is_query(&subquery.this) => Some(&subquery.this),
        Expression::Paren(paren) => inner_query(&paren.this),
        // DML statements are handled structurally; their nested queries
        // (WHERE subqueries, MERGE sources) are not "the" query.
        Expression::Update(_) | Expression::Delete(_) | Expression::Merge(_) => None,
        _ => statement.children().into_iter().find_map(|child| {
            if is_query(child) {
                Some(child)
            } else if let Expression::Subquery(subquery) = child {
                is_query(&subquery.this).then_some(&subquery.this)
            } else {
                None
            }
        }),
    }
}

/// Runs `f`, first growing the stack when little of it is left. Wraps every
/// recursive AST walk so deep (but parser-accepted) input cannot overflow
/// native stacks. Without the `stacker` feature (WebAssembly) the module's
/// stack is sized for the parser's depth limit instead.
#[inline]
pub(crate) fn with_stack<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(feature = "stacker")]
    {
        stacker::maybe_grow(128 * 1024, 4 * 1024 * 1024, f)
    }
    #[cfg(not(feature = "stacker"))]
    {
        f()
    }
}

/// Runs `f` on a fresh, large stack segment (native builds) for library
/// calls that recurse deeply without growing the stack themselves.
pub(crate) fn with_large_stack<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(feature = "stacker")]
    {
        stacker::maybe_grow(8 * 1024 * 1024, 64 * 1024 * 1024, f)
    }
    #[cfg(not(feature = "stacker"))]
    {
        f()
    }
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Whether a caller-supplied identifier must be quoted to keep its exact
/// value: anything other than a simple lower-case identifier. Mixed or upper
/// case is quoted because folding dialects would otherwise change it.
pub(crate) fn needs_quote(name: &str) -> bool {
    let mut chars = name.chars();
    let simple = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    !simple || name.chars().any(|c| c.is_ascii_uppercase())
}

/// An identifier node for a caller-supplied name.
pub(crate) fn ident(name: &str) -> Identifier {
    ident_with(name, needs_quote(name))
}

pub(crate) fn ident_with(name: &str, quoted: bool) -> Identifier {
    if quoted {
        Identifier::quoted(name)
    } else {
        Identifier::new(name)
    }
}

/// The comparison key for an identifier: SQL folds only unquoted names, so
/// quoted identifiers compare exactly and unquoted ones case-insensitively.
pub(crate) fn ident_key(identifier: &Identifier) -> String {
    if identifier.quoted {
        identifier.name.clone()
    } else {
        identifier.name.to_lowercase()
    }
}

/// The name of an optional identifier, or `""`.
pub(crate) fn opt_name(identifier: &Option<Identifier>) -> &str {
    identifier.as_ref().map_or("", |i| i.name.as_str())
}

/// `catalog.schema.table` as written (without quotes), omitting absent parts.
pub(crate) fn qualified_name(table: &TableRef) -> String {
    let mut parts = Vec::with_capacity(3);
    if let Some(catalog) = table.catalog.as_ref().filter(|catalog| !catalog.name.is_empty()) {
        parts.push(catalog.name.as_str());
        // T-SQL `db..t` names the default schema of `db`: kept as `db..t`,
        // which `db.t` (schema `db`) would misstate.
        parts.push(opt_name(&table.schema));
    } else if let Some(schema) = table.schema.as_ref().filter(|schema| !schema.name.is_empty()) {
        parts.push(schema.name.as_str());
    }
    parts.push(table.name.name.as_str());
    parts.join(".")
}

/// Whether `qualified` names the table `reference` on a dot boundary: equal,
/// or `qualified` ends with `.reference`. `raw.orders` matches
/// `hive.raw.orders` but not `hive.braw.orders`.
pub(crate) fn has_table_suffix(qualified: &str, reference: &str) -> bool {
    if reference.is_empty() {
        return false;
    }
    qualified == reference
        || (qualified.len() > reference.len()
            && qualified.ends_with(reference)
            && qualified.as_bytes()[qualified.len() - reference.len() - 1] == b'.')
}

pub(crate) fn table_names_match(a: &str, b: &str) -> bool {
    has_table_suffix(a, b) || has_table_suffix(b, a)
}

// ---------------------------------------------------------------------------
// Node identities and post-order edits
// ---------------------------------------------------------------------------

/// Stable identities for the nodes of an immutable tree.
///
/// Analysis passes borrow the tree and refer to nodes by address; [`NodeIds`]
/// translates those addresses into post-order positions, which is exactly
/// the order in which [`transform_all`] visits nodes. Edits planned against a
/// borrowed tree can therefore be applied to the owned tree afterwards.
pub(crate) struct NodeIds {
    ids: HashMap<*const Expression, usize>,
    kinds: Vec<Discriminant<Expression>>,
}

impl NodeIds {
    pub(crate) fn new(root: &Expression) -> Self {
        let mut ids = HashMap::new();
        let mut kinds = Vec::new();
        // Iterative post-order: (node, children already expanded).
        let mut stack: Vec<(&Expression, bool)> = vec![(root, false)];
        while let Some((node, expanded)) = stack.pop() {
            if expanded {
                ids.insert(node as *const Expression, kinds.len());
                kinds.push(discriminant(node));
                continue;
            }
            stack.push((node, true));
            let children = node.children();
            for child in children.into_iter().rev() {
                stack.push((child, false));
            }
        }
        Self { ids, kinds }
    }

    pub(crate) fn id(&self, node: &Expression) -> Option<usize> {
        self.ids.get(&(node as *const Expression)).copied()
    }
}

type Edit = Box<dyn FnOnce(Expression) -> Result<Expression>>;

/// A set of node replacements planned against a borrowed tree.
#[derive(Default)]
pub(crate) struct Edits {
    edits: HashMap<usize, Edit>,
}

impl Edits {
    /// Plans replacing `node` (a node of the tree `ids` was built from).
    pub(crate) fn replace(
        &mut self,
        ids: &NodeIds,
        node: &Expression,
        edit: impl FnOnce(Expression) -> Result<Expression> + 'static,
    ) -> Result<()> {
        let id = ids
            .id(node)
            .ok_or_else(|| Error::internal("edit target is not part of the statement"))?;
        if self.edits.insert(id, Box::new(edit)).is_some() {
            return Err(Error::internal("conflicting edits for one AST node"));
        }
        Ok(())
    }

    /// Applies the edits to the tree `ids` was built from. Fails closed if the
    /// traversal does not line up with the planned node identities.
    pub(crate) fn apply(self, root: Expression, ids: &NodeIds) -> Result<Expression> {
        let position = Cell::new(0usize);
        let edits = RefCell::new(self.edits);
        let failure = RefCell::new(None::<Error>);
        let result = transform_all(root, &|node| {
            let id = position.get();
            position.set(id + 1);
            if ids.kinds.get(id) != Some(&discriminant(&node)) {
                failure.replace(Some(Error::internal("AST traversal order mismatch")));
                return Ok(node);
            }
            let Some(edit) = edits.borrow_mut().remove(&id) else {
                return Ok(node);
            };
            match edit(node) {
                Ok(replacement) => Ok(replacement),
                Err(error) => {
                    failure.replace(Some(error));
                    Ok(Expression::Null(polyglot_sql::expressions::Null))
                }
            }
        })
        .map_err(|error| Error::internal(format!("AST transform failed: {error}")))?;
        if let Some(error) = failure.into_inner() {
            return Err(error);
        }
        if position.get() != ids.kinds.len() || !edits.borrow().is_empty() {
            return Err(Error::internal("AST traversal did not visit every planned edit"));
        }
        Ok(result)
    }
}
