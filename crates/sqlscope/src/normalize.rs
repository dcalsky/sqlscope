//! Dialect-specific input normalization applied before parsing.

use std::borrow::Cow;

use polyglot_sql::TokenType;

use crate::ast;
use crate::error::Result;
use crate::options::Dialect;

/// Rewrites input the parser cannot read into an equivalent form.
///
/// StarRocks join distribution hints (`JOIN [broadcast] t`) are not understood
/// by the parser. They only influence the physical plan, so they are dropped
/// from code (never from string literals, quoted identifiers or comments).
pub(crate) fn normalize(sql: &str, dialect: Dialect) -> Cow<'_, str> {
    if sql.len() > ast::MAX_INPUT_BYTES || dialect != Dialect::STARROCKS || !sql.contains('[') {
        return Cow::Borrowed(sql);
    }
    let Ok(tokens) = polyglot_sql::Dialect::get(dialect.polyglot()).tokenize(sql) else {
        return Cow::Borrowed(sql);
    };
    let mut bytes = None;
    let mut out = String::new();
    let mut previous = 0;
    for group in tokens.windows(4) {
        let [join, open, _, close] = group else { continue };
        if (join.token_type, open.token_type, close.token_type)
            != (TokenType::Join, TokenType::LBracket, TokenType::RBracket)
        {
            continue;
        }
        let bytes = bytes.get_or_insert_with(|| byte_offsets(sql));
        let hint = sql[bytes[open.span.end]..bytes[close.span.start]].trim();
        if ["broadcast", "shuffle", "bucket_shuffle", "colocate", "replicated"]
            .iter()
            .any(|name| hint.eq_ignore_ascii_case(name))
        {
            out.push_str(&sql[previous..bytes[open.span.start]]);
            previous = bytes[close.span.end];
            if join.span.end == open.span.start && !sql[previous..].starts_with(char::is_whitespace) {
                out.push(' ');
            }
        }
    }
    if previous == 0 {
        return Cow::Borrowed(sql);
    }
    out.push_str(&sql[previous..]);
    Cow::Owned(out)
}

pub(crate) fn byte_offsets(sql: &str) -> Vec<usize> {
    sql.char_indices().map(|(at, _)| at).chain([sql.len()]).collect()
}

/// Validate the original input before normalization can erase dialect-specific comments.
pub(crate) fn prepare_rewrite(sql: &str, dialect: Dialect) -> Result<Cow<'_, str>> {
    ast::guard_input(sql)?;
    ast::guard_comments(sql, dialect)?;
    Ok(match normalize(sql, dialect) {
        Cow::Borrowed(sql) => strip_comments(sql, dialect),
        Cow::Owned(sql) => Cow::Owned(strip_comments(&sql, dialect).into_owned()),
    })
}

/// Removes comments from `sql` using the dialect's tokenizer, so rewritten
/// output regenerated from the AST carries none. Optimizer hints
/// (`/*+ ... */`) are tokens, not comments, and are kept. Input the tokenizer
/// rejects is returned unchanged for the parser to report.
fn strip_comments(sql: &str, dialect: Dialect) -> Cow<'_, str> {
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
    let bytes = byte_offsets(sql);
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
    fn keeps_subscripts_named_like_hints() {
        let sql = "select arr[shuffle], m[ Broadcast ] from a join[colocate] b";
        assert_eq!(
            normalize(sql, Dialect::STARROCKS),
            "select arr[shuffle], m[ Broadcast ] from a join b"
        );
    }

    #[test]
    fn compact_hints_preserve_unicode_spans_and_comment_text() {
        let sql = "SELECT '甲 JOIN[shuffle]表' FROM 订单 JOIN[shuffle]行 /* JOIN[broadcast]别名 */ JOIN[COLOCATE]表";
        assert_eq!(
            normalize(sql, Dialect::STARROCKS),
            "SELECT '甲 JOIN[shuffle]表' FROM 订单 JOIN 行 /* JOIN[broadcast]别名 */ JOIN 表"
        );
    }

    #[test]
    fn rewrite_preparation_checks_comment_syntax_before_removal() {
        for dialect in [Dialect::MYSQL, Dialect::STARROCKS] {
            assert_eq!(
                prepare_rewrite("SELECT 1 /*! */", dialect).unwrap_err().kind(),
                crate::ErrorKind::Unsupported
            );
            assert_eq!(
                prepare_rewrite("SELECT 1 /* note */", dialect).unwrap().trim(),
                "SELECT 1"
            );
        }
        assert_eq!(
            prepare_rewrite("SELECT 1 /*! */", Dialect::TRINO).unwrap().trim(),
            "SELECT 1"
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
