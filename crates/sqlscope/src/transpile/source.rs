//! Trino query primaries and expression grammar missing from the shared parser.
//! All edits use tokenizer spans. Strings and quoted names remain opaque.
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use polyglot_sql::{Dialect as ParserDialect, DialectType, Expression, Token, TokenType};

use super::rendered;
use crate::{Dialect, Error, Result};

#[derive(Debug)]
enum Extension {
    Excluding,
    JsonValue(Vec<Behavior>),
}

#[derive(Debug)]
struct Behavior {
    kind: String,
    condition: String,
}

pub(super) struct Source<'a> {
    pub sql: Cow<'a, str>,
    extensions: HashMap<String, Extension>,
}

impl Source<'_> {
    /// Restore typed extension nodes after the shared parser has parsed and
    /// lowered their operands. Marker names are unique within this input.
    pub fn restore(&self, expr: Expression, target: Dialect) -> polyglot_sql::Result<Expression> {
        let Expression::Function(f) = &expr else {
            return Ok(expr);
        };
        let Some(extension) = self.extensions.get(&f.name.to_ascii_uppercase()) else {
            return Ok(expr);
        };
        let mut args = f.args.iter();
        let invalid = || polyglot_sql::Error::unsupported("invalid Trino extension operands", "trino");
        let first = args.next().ok_or_else(invalid)?;
        let sql = |node: &Expression| polyglot_sql::generate(node, target.polyglot());
        match extension {
            Extension::Excluding => Ok(rendered(format!("EXCLUDING {}", sql(first)?))),
            Extension::JsonValue(behaviors) => {
                let mut value = sql(first)?;
                if value.pop() != Some(')') {
                    return Err(invalid());
                }
                for behavior in behaviors {
                    value.push(' ');
                    value.push_str(&behavior.kind);
                    if behavior.kind == "DEFAULT" {
                        value.push(' ');
                        value.push_str(&sql(args.next().ok_or_else(invalid)?)?);
                    }
                    value.push_str(" ON ");
                    value.push_str(&behavior.condition);
                }
                value.push(')');
                Ok(rendered(value))
            }
        }
    }
}

struct Grammar<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
    mate: Vec<usize>,
    edits: Vec<(usize, usize, String)>,
    names: HashSet<String>,
    next_id: usize,
    extensions: HashMap<String, Extension>,
}

pub(super) fn parse_source(sql: &str) -> Result<Source<'_>> {
    let mut tokens = ParserDialect::get(DialectType::Trino)
        .tokenize(sql)
        .map_err(|e| Error::parse(e.to_string()))?;
    // The tokenizer reports Unicode scalar offsets. The edits below slice UTF-8.
    if !sql.is_ascii() {
        let bytes = crate::normalize::byte_offsets(sql);
        for token in &mut tokens {
            token.span.start = bytes[token.span.start];
            token.span.end = bytes[token.span.end];
        }
    }
    let mut mate = vec![0; tokens.len()];
    let mut stack = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        match token.token_type {
            TokenType::LParen => stack.push(i),
            TokenType::RParen => {
                let open = stack
                    .pop()
                    .ok_or_else(|| Error::parse("unexpected closing parenthesis"))?;
                mate[open] = i;
                mate[i] = open;
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(Error::parse("unclosed parenthesis"));
    }
    let names = tokens
        .iter()
        .filter(|t| matches!(t.token_type, TokenType::Var | TokenType::QuotedIdentifier))
        .map(|t| t.text.to_ascii_uppercase())
        .collect();
    let mut grammar = Grammar {
        sql,
        tokens,
        mate,
        edits: Vec::new(),
        names,
        next_id: 0,
        extensions: HashMap::new(),
    };
    grammar.set_precedence();
    let mut start = 0;
    for i in 0..grammar.tokens.len() {
        match grammar.kind(i) {
            Some(TokenType::Semicolon) => start = i + 1,
            Some(TokenType::Table) => grammar.table_primary(i, start),
            Some(TokenType::Apply) if i > 0 && grammar.kind(i - 1) == Some(TokenType::RParen) => {
                let open = grammar.mate[i - 1];
                if open > 0
                    && matches!(
                        grammar.kind(open - 1),
                        Some(TokenType::From | TokenType::Join | TokenType::Comma)
                    )
                {
                    let at = grammar.tokens[i].span.start;
                    grammar.edits.push((at, at, "AS ".into()));
                }
            }
            Some(TokenType::Var) if grammar.word(i, "JSON_VALUE") && grammar.kind(i + 1) == Some(TokenType::LParen) => {
                grammar.json_value(i)?
            }
            Some(TokenType::Var)
                if grammar.word(i, "EXCLUDING")
                    && i >= 2
                    && grammar.kind(i - 2) == Some(TokenType::Partition)
                    && grammar.kind(i - 1) == Some(TokenType::By) =>
            {
                grammar.excluding(i)
            }
            Some(TokenType::Var)
                if i > 0
                    && grammar.kind(i - 1) == Some(TokenType::As)
                    && (grammar.word(i, "DECIMAL") || grammar.word(i, "NUMERIC")) =>
            {
                grammar.decimal(i)
            }
            _ => {}
        }
    }
    grammar.edits.sort_by_key(|(start, end, _)| (*start, *end));
    let mut output = String::new();
    let mut cursor = 0;
    for (start, end, replacement) in grammar.edits {
        if start < cursor {
            return Err(Error::parse("overlapping Trino grammar clauses"));
        }
        output.push_str(&sql[cursor..start]);
        output.push_str(&replacement);
        cursor = end;
    }
    let sql = if output.is_empty() && cursor == 0 {
        Cow::Borrowed(sql)
    } else {
        output.push_str(&sql[cursor..]);
        Cow::Owned(output)
    };
    Ok(Source {
        sql,
        extensions: grammar.extensions,
    })
}

impl Grammar<'_> {
    fn kind(&self, i: usize) -> Option<TokenType> {
        self.tokens.get(i).map(|t| t.token_type)
    }
    fn word(&self, i: usize, text: &str) -> bool {
        self.tokens.get(i).is_some_and(|t| t.text.eq_ignore_ascii_case(text))
    }
    fn marker(&mut self, extension: Extension) -> String {
        loop {
            let name = format!("__SQLSCOPE_TRINO_{}", self.next_id);
            self.next_id += 1;
            if !self.names.contains(&name) {
                self.extensions.insert(name.clone(), extension);
                return name;
            }
        }
    }
    fn table_primary(&mut self, i: usize, start: usize) {
        if !matches!(self.kind(i + 1), Some(TokenType::Var | TokenType::QuotedIdentifier)) {
            return;
        }
        let previous = i.checked_sub(1).and_then(|p| self.kind(p));
        if i == start
            || matches!(
                previous,
                Some(TokenType::LParen | TokenType::Union | TokenType::Except | TokenType::Intersect | TokenType::As)
            )
            || (matches!(previous, Some(TokenType::All | TokenType::Distinct))
                && i >= 2
                && matches!(
                    self.kind(i - 2),
                    Some(TokenType::Union | TokenType::Except | TokenType::Intersect)
                ))
            || (self.kind(start) == Some(TokenType::With) && previous == Some(TokenType::RParen))
            || (self.kind(start) == Some(TokenType::Insert)
                && matches!(
                    previous,
                    Some(TokenType::Var | TokenType::QuotedIdentifier | TokenType::RParen)
                ))
        {
            let span = self.tokens[i].span;
            let next = self.tokens[i + 1].span.start;
            let trivia = &self.sql[span.end..next];
            self.edits.push((span.start, next, format!("{trivia}SELECT * FROM ")));
        }
    }
    fn decimal(&mut self, i: usize) {
        if self.kind(i + 1) != Some(TokenType::LParen) {
            return;
        }
        let close = self.mate[i + 1];
        let args = &self.tokens[i + 2..close];
        if !matches!(args.len(), 1 | 3) || (args.len() == 3 && args[1].token_type != TokenType::Comma) {
            return;
        }
        let params: Vec<_> = args.iter().step_by(2).collect();
        if !params.iter().any(|p| p.token_type == TokenType::Var)
            || !params.iter().all(|p| {
                matches!(p.token_type, TokenType::Var | TokenType::Number)
                    && p.text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        {
            return;
        }
        let params = params
            .iter()
            .map(|p| p.text.to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(", ");
        self.edits.push((
            self.tokens[i].span.start,
            self.tokens[close].span.end,
            format!("\"DECIMAL({params})\""),
        ));
    }
    fn excluding(&mut self, i: usize) {
        if !matches!(self.kind(i + 1), Some(TokenType::Var | TokenType::QuotedIdentifier)) {
            return;
        }
        let mut last = i + 1;
        while self.kind(last + 1) == Some(TokenType::Dot)
            && matches!(self.kind(last + 2), Some(TokenType::Var | TokenType::QuotedIdentifier))
        {
            last += 2;
        }
        let name = self.marker(Extension::Excluding);
        let span = self.tokens[i].span;
        self.edits.push((span.start, span.end, format!("{name}(")));
        let end = self.tokens[last].span.end;
        self.edits.push((end, end, ")".into()));
    }
    fn json_value(&mut self, i: usize) -> Result<()> {
        let close = self.mate[i + 1];
        let mut top = Vec::new();
        let mut j = i + 2;
        while j < close {
            top.push(j);
            j = if self.kind(j) == Some(TokenType::LParen) {
                self.mate[j] + 1
            } else {
                j + 1
            };
        }
        let mut behaviors = Vec::new();
        let mut previous = 0;
        for (pos, &on) in top.iter().enumerate() {
            if self.kind(on) != Some(TokenType::On) || !(self.word(on + 1, "ERROR") || self.word(on + 1, "EMPTY")) {
                continue;
            }
            let Some(first) = top[previous..pos]
                .iter()
                .rposition(|&t| self.kind(t) == Some(TokenType::Default))
                .map(|p| top[previous + p])
                .or_else(|| pos.checked_sub(1).map(|p| top[p]))
            else {
                return Err(Error::parse("missing JSON_VALUE behavior"));
            };
            let kind = self.tokens[first].text.to_ascii_uppercase();
            if !matches!(kind.as_str(), "DEFAULT" | "NULL" | "ERROR") || (kind == "DEFAULT" && first + 1 == on) {
                return Err(Error::parse("invalid JSON_VALUE behavior"));
            }
            let condition = self.tokens[on + 1].text.to_ascii_uppercase();
            if behaviors.iter().any(|b: &Behavior| b.condition == condition) {
                return Err(Error::parse("duplicate JSON_VALUE behavior"));
            }
            let mut replacement = if behaviors.is_empty() {
                ")".to_owned()
            } else {
                String::new()
            };
            if kind == "DEFAULT" {
                replacement.push_str(", ");
            }
            self.edits
                .push((self.tokens[first].span.start, self.tokens[first].span.end, replacement));
            self.edits
                .push((self.tokens[on].span.start, self.tokens[on + 1].span.end, String::new()));
            behaviors.push(Behavior { kind, condition });
            previous = pos + 2;
        }
        if !behaviors.is_empty() {
            let name = self.marker(Extension::JsonValue(behaviors));
            let at = self.tokens[i].span.start;
            self.edits.push((at, at, format!("{name}(")));
        }
        Ok(())
    }

    /// INTERSECT binds tighter than UNION and EXCEPT in Trino. TokenType::Insert grouping
    /// around each affected right operand, ahead of query-wide modifiers.
    fn set_precedence(&mut self) {
        let mut levels = vec![(None, false)];
        for i in 0..=self.tokens.len() {
            let kind = self.kind(i);
            let (begin, active) = levels.last_mut().unwrap();
            if matches!(
                kind,
                None | Some(
                    TokenType::Union
                        | TokenType::Except
                        | TokenType::Order
                        | TokenType::Limit
                        | TokenType::Offset
                        | TokenType::Fetch
                        | TokenType::Semicolon
                        | TokenType::RParen
                )
            ) {
                if *active {
                    let at = self.tokens[i - 1].span.end;
                    self.edits.push((at, at, ")".into()));
                }
                *active = false;
                *begin = None;
            }
            match kind {
                Some(TokenType::Union | TokenType::Except) => {
                    let operand = i + if matches!(self.kind(i + 1), Some(TokenType::All | TokenType::Distinct)) {
                        2
                    } else {
                        1
                    };
                    *begin = self.tokens.get(operand).map(|t| t.span.start);
                }
                Some(TokenType::Intersect) if !*active => {
                    if let Some(at) = *begin {
                        self.edits.push((at, at, "(".into()));
                        *active = true;
                    }
                }
                Some(TokenType::LParen) => levels.push((None, false)),
                Some(TokenType::RParen) => {
                    levels.pop();
                }
                _ => {}
            }
        }
    }
}
