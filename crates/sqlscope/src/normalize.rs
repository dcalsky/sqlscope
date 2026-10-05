//! Dialect-specific input normalization applied before parsing.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use crate::options::Dialect;

static JOIN_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\[\s*(broadcast|shuffle|bucket_shuffle|colocate|replicated)\s*\]")
        .expect("valid join hint pattern")
});

/// Rewrites input the parser cannot read into an equivalent form.
///
/// StarRocks join distribution hints (`JOIN [broadcast] t`) are not understood
/// by the parser. They only influence the physical plan, so they are dropped
/// from code (never from string literals, quoted identifiers or comments).
pub(crate) fn normalize(sql: &str, dialect: Dialect) -> Cow<'_, str> {
    if dialect == Dialect::STARROCKS && sql.contains('[') {
        Cow::Owned(map_code_segments(sql, |code| {
            JOIN_HINT.replace_all(code, "").into_owned()
        }))
    } else {
        Cow::Borrowed(sql)
    }
}

/// Input preparation for rewrites: [`normalize`] plus [`strip_comments`].
pub(crate) fn prepare_rewrite(sql: &str, dialect: Dialect) -> Cow<'_, str> {
    match normalize(sql, dialect) {
        Cow::Borrowed(sql) => strip_comments(sql, dialect),
        Cow::Owned(sql) => Cow::Owned(strip_comments(&sql, dialect).into_owned()),
    }
}

/// Removes comments from `sql` using the dialect's tokenizer, so rewritten
/// output regenerated from the AST carries none. Optimizer hints
/// (`/*+ ... */`) are tokens, not comments, and are kept. Input the tokenizer
/// rejects is returned unchanged for the parser to report.
pub(crate) fn strip_comments(sql: &str, dialect: Dialect) -> Cow<'_, str> {
    if sql.len() > crate::ast::MAX_INPUT_BYTES {
        return Cow::Borrowed(sql); // rejected by the parser's input guard
    }
    let Ok(tokens) = polyglot_sql::Dialect::get(dialect.polyglot()).tokenize(sql) else {
        return Cow::Borrowed(sql);
    };
    let commented = |token: &polyglot_sql::Token| !token.comments.is_empty() || !token.trailing_comments.is_empty();
    if !tokens.iter().any(commented) {
        return Cow::Borrowed(sql);
    }
    // Token spans are character offsets.
    let mut bytes: Vec<usize> = sql.char_indices().map(|(at, _)| at).collect();
    bytes.push(sql.len());
    let byte = |chars: usize| bytes.get(chars).copied().unwrap_or(sql.len());

    let mut out = String::with_capacity(sql.len());
    let mut previous_end = 0;
    let mut previous_commented = false;
    for token in &tokens {
        let (start, end) = (byte(token.span.start), byte(token.span.end));
        if start < previous_end || end < start {
            return Cow::Borrowed(sql); // unexpected spans: leave input alone
        }
        let gap = &sql[previous_end..start];
        if previous_commented || !token.comments.is_empty() {
            if !out.is_empty() {
                out.push(if gap.contains('\n') { '\n' } else { ' ' });
            }
        } else {
            out.push_str(gap);
        }
        out.push_str(&sql[start..end]);
        previous_end = end;
        previous_commented = !token.trailing_comments.is_empty();
    }
    if !previous_commented {
        out.push_str(&sql[previous_end..]);
    }
    Cow::Owned(out)
}

/// Applies `transform` to the parts of `sql` outside string literals, quoted
/// identifiers and comments.
fn map_code_segments(sql: &str, transform: impl Fn(&str) -> String) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut code_start = 0;
    let mut i = 0;
    let flush = |out: &mut String, from: usize, to: usize| {
        if from < to {
            out.push_str(&transform(&sql[from..to]));
        }
    };
    while i < bytes.len() {
        let end = match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => {
                let mut j = i + 1;
                loop {
                    match bytes.get(j) {
                        None => break bytes.len(),
                        Some(b'\\') if quote != b'`' && j + 1 < bytes.len() => j += 2,
                        Some(&c) if c == quote => {
                            if bytes.get(j + 1) == Some(&quote) {
                                j += 2; // doubled-quote escape
                            } else {
                                break j + 1;
                            }
                        }
                        Some(_) => j += 1,
                    }
                }
            }
            b'-' if sql[i..].starts_with("-- ") => line_end(bytes, i),
            b'#' => line_end(bytes, i),
            b'/' if sql[i..].starts_with("/*") => {
                sql[i + 2..].find("*/").map_or(bytes.len(), |offset| i + 2 + offset + 2)
            }
            _ => {
                i += 1;
                continue;
            }
        };
        flush(&mut out, code_start, i);
        out.push_str(&sql[i..end]);
        i = end;
        code_start = end;
    }
    flush(&mut out, code_start, bytes.len());
    out
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |offset| from + offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_join_hints_only_in_code() {
        let sql = "select '[broadcast]', `[shuffle]` from a join [broadcast] b -- [colocate]\n join [ Shuffle ] c";
        assert_eq!(
            normalize(sql, Dialect::STARROCKS),
            "select '[broadcast]', `[shuffle]` from a join  b -- [colocate]\n join  c"
        );
    }

    #[test]
    fn strips_comments_but_not_hints_or_literals() {
        let sql = "/* lead */ select /*+ SET_VAR(x=5) */ 'a -- b', $$c /* d */$$ -- tail\n from é /* e */ where x = 1";
        assert_eq!(
            strip_comments(sql, Dialect::POSTGRES),
            "select /*+ SET_VAR(x=5) */ 'a -- b', $$c /* d */$$\nfrom é where x = 1"
        );
        let plain = "select 1";
        assert!(matches!(strip_comments(plain, Dialect::TRINO), Cow::Borrowed(_)));
    }

    #[test]
    fn leaves_other_dialects_untouched() {
        let sql = "select * from a join [broadcast] b";
        assert!(matches!(normalize(sql, Dialect::MYSQL), Cow::Borrowed(_)));
    }
}
