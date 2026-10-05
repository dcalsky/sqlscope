use std::collections::HashMap;

use polyglot_sql::expressions::{Cte, With};

use crate::ast::{self, ident, ident_key, query_with_mut, MAX_INPUT_BYTES};
use crate::error::{Error, Result};
use crate::normalize::prepare_rewrite;
use crate::options::Options;

/// A common table expression to inject: `name AS (query)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CteDef {
    /// The CTE name. An identifier value, not SQL text: it is quoted for the
    /// dialect as needed.
    pub name: String,
    /// A single `SELECT` or set operation.
    pub query: String,
}

impl CteDef {
    pub fn new(name: impl Into<String>, query: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            query: query.into(),
        }
    }
}

/// Adds CTE definitions to the root `WITH` clause of a query.
///
/// Definitions keep their order and are placed before the CTEs the query
/// already declares, so each definition may use earlier ones and existing
/// CTEs may use any of them. A name that repeats another definition or an
/// existing root CTE is rejected. Simple names compare case-insensitively.
///
/// The query and every definition must each be a single `SELECT` or set
/// operation. Composition is structural (AST, not text). Output is
/// regenerated (normalized, comments dropped) and verified to parse. An empty
/// `ctes` slice returns the input unchanged without validating it.
///
/// ```
/// use sqlscope::{inject_ctes, CteDef, Options};
///
/// let sql = inject_ctes(
///     "SELECT id FROM recent",
///     &[CteDef::new("recent", "SELECT id FROM orders WHERE day > 7")],
///     &Options::new(),
/// )?;
/// assert_eq!(sql, "WITH recent AS (SELECT id FROM orders WHERE day > 7) SELECT id FROM recent");
/// # Ok::<(), sqlscope::Error>(())
/// ```
pub fn inject_ctes(sql: &str, ctes: &[CteDef], options: &Options) -> Result<String> {
    if ctes.is_empty() {
        return Ok(sql.to_owned());
    }
    let dialect = options.dialect;

    let mut total = sql.len();
    let mut names: HashMap<String, &str> = HashMap::with_capacity(ctes.len());
    for (index, cte) in ctes.iter().enumerate() {
        let name = cte.name.trim();
        if name.is_empty() {
            return Err(Error::invalid(format!("CTE {index} name must not be empty")));
        }
        if cte.query.trim().is_empty() {
            return Err(Error::invalid(format!("CTE {name:?} query must not be empty")));
        }
        total = total.saturating_add(cte.query.len());
        if total > MAX_INPUT_BYTES {
            return Err(Error::unsupported(format!(
                "combined input too large (more than {MAX_INPUT_BYTES} bytes)"
            )));
        }
        if names.insert(name_key(name), name).is_some() {
            return Err(Error::invalid(format!("duplicate CTE {name:?}")));
        }
    }

    let mut consumer =
        ast::parse_query(&prepare_rewrite(sql, dialect), dialect).map_err(|error| error.context("query"))?;
    let slot = query_with_mut(&mut consumer).ok_or_else(|| Error::internal("query has no WITH slot"))?;
    if let Some(existing) = slot {
        for cte in &existing.ctes {
            if let Some(name) = names.get(&ident_key(&cte.alias)) {
                return Err(Error::invalid(format!(
                    "CTE {name:?} conflicts with existing CTE {:?}",
                    cte.alias.name
                )));
            }
        }
    }

    let mut injected = Vec::with_capacity(ctes.len());
    for cte in ctes {
        let name = cte.name.trim();
        let query = ast::parse_query(&prepare_rewrite(&cte.query, dialect), dialect)
            .map_err(|error| error.context(format!("CTE {name:?}")))?;
        injected.push(Cte {
            alias: ident(name),
            this: query,
            columns: Vec::new(),
            materialized: None,
            key_expressions: Vec::new(),
            alias_first: true,
            comments: Vec::new(),
        });
    }

    match slot {
        Some(existing) => {
            injected.append(&mut existing.ctes);
            existing.ctes = injected;
        }
        None => {
            *slot = Some(With {
                ctes: injected,
                recursive: false,
                leading_comments: Vec::new(),
                search: None,
            })
        }
    }
    ast::generate_checked(&consumer, dialect)
}

/// Duplicate detection key: simple names compare like unquoted SQL names
/// (case-insensitively); anything else, which is emitted quoted, exactly.
fn name_key(name: &str) -> String {
    let simple = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if simple {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}
