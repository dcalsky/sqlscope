//! Lookups in the caller-supplied `table -> columns` schema.

use std::collections::BTreeMap;

use polyglot_sql::expressions::{Identifier, TableRef};
use polyglot_sql::optimizer::normalize_identifiers::{
    get_normalization_strategy, normalize_identifier, NormalizationStrategy,
};
use polyglot_sql::{SchemaColumn, SchemaTable, ValidationSchema};

use crate::ast::has_table_suffix;
use crate::options::Dialect;

pub(crate) type Schema = BTreeMap<String, Vec<String>>;

/// Catalog spelling is significant in dialects that support quoted,
/// case-sensitive names. In case-insensitive dialects both sides fold.
pub(crate) struct SchemaLookup<'s> {
    tables: BTreeMap<String, Option<&'s [String]>>,
    table_strategy: NormalizationStrategy,
    column_strategy: NormalizationStrategy,
}

impl<'s> SchemaLookup<'s> {
    pub(crate) fn new(schema: &'s Schema, dialect: Dialect) -> Self {
        use NormalizationStrategy::*;
        let strategy = get_normalization_strategy(Some(dialect.polyglot()));
        // BigQuery's default dataset configuration has case-sensitive tables;
        // MySQL's case-sensitive table setting does not apply to columns.
        let table_strategy = if dialect == Dialect::BIGQUERY {
            CaseSensitive
        } else {
            strategy
        };
        let column_strategy = if dialect == Dialect::MYSQL {
            CaseInsensitive
        } else {
            strategy
        };
        let mut tables = BTreeMap::new();
        for (key, columns) in schema {
            let key = normalize_identifier(Identifier::quoted(key), table_strategy).name;
            // Conflicting declarations of the same table are unknown.
            let entry = tables.entry(key).or_insert(Some(columns.as_slice()));
            if *entry != Some(columns.as_slice()) {
                *entry = None;
            }
        }
        Self {
            tables,
            table_strategy,
            column_strategy,
        }
    }

    pub(crate) fn columns(&self, table: &TableRef) -> Option<&'s [String]> {
        if self.tables.is_empty() {
            return None;
        }
        match self.lookup(table, self.table_strategy) {
            Some(columns) => columns,
            // Uppercase-folding dialects have historically also accepted
            // lowercase schema maps. Apply that convention only to unquoted
            // parts, independent of the case used to spell them in SQL.
            None if self.table_strategy == NormalizationStrategy::Uppercase => {
                self.lookup(table, NormalizationStrategy::Lowercase).flatten()
            }
            None => None,
        }
    }

    // Outer None means no match; Some(None) means ambiguous. In particular,
    // an ambiguous match must not select a different fallback spelling.
    fn lookup(&self, table: &TableRef, strategy: NormalizationStrategy) -> Option<Option<&'s [String]>> {
        let normalized = |id: &Identifier| normalize_identifier(id.clone(), strategy).name;
        let mut parts = Vec::with_capacity(3);
        if let Some(catalog) = &table.catalog {
            parts.push(normalized(catalog));
            // Keep the empty schema in T-SQL db..table.
            parts.push(table.schema.as_ref().map(normalized).unwrap_or_default());
        } else if let Some(schema) = &table.schema {
            parts.push(normalized(schema));
        }
        parts.push(normalized(&table.name));
        let key = parts.join(".");
        if let Some(columns) = self.tables.get(&key) {
            return Some(*columns);
        }
        let mut candidates = self
            .tables
            .iter()
            .filter(|(candidate, _)| has_table_suffix(candidate, &key));
        match (candidates.next(), candidates.next()) {
            (Some((_, columns)), None) => Some(*columns),
            (Some(_), Some(_)) => Some(None),
            // Drop whole SQL identifiers so a dot inside a quoted name
            // cannot turn a different table into a suffix match.
            _ => (1..parts.len()).find_map(|start| self.tables.get(&parts[start..].join(".")).copied()),
        }
    }

    pub(crate) fn has_column(&self, columns: &[String], id: &Identifier) -> bool {
        use NormalizationStrategy::*;
        let key = normalize_identifier(id.clone(), self.column_strategy).name;
        match self.column_strategy {
            CaseInsensitive | CaseInsensitiveUppercase => columns.iter().any(|column| {
                if column.is_ascii() {
                    column.eq_ignore_ascii_case(&key)
                } else {
                    normalize_identifier(Identifier::quoted(column), self.column_strategy).name == key
                }
            }),
            Uppercase if !id.quoted => {
                let lower = id.name.to_lowercase();
                columns.iter().any(|column| column == &key || column == &lower)
            }
            _ => columns.contains(&key),
        }
    }
}

/// Converts the schema into polyglot's validation schema. A dotted name is
/// split at its last dot: `hive.raw.orders` is table `orders` in schema
/// `hive.raw`.
pub(crate) fn to_validation_schema(schema: &Schema) -> Option<ValidationSchema> {
    if schema.is_empty() {
        return None;
    }
    let tables = schema
        .iter()
        .map(|(full, columns)| {
            let (namespace, name) = match full.rsplit_once('.') {
                Some((namespace, name)) => (Some(namespace.to_owned()), name.to_owned()),
                None => (None, full.clone()),
            };
            SchemaTable {
                name,
                schema: namespace,
                columns: columns
                    .iter()
                    .map(|column| SchemaColumn {
                        name: column.clone(),
                        // Only names are known; the type affects neither
                        // wildcard expansion nor lineage.
                        data_type: "VARCHAR".to_owned(),
                        nullable: None,
                        primary_key: false,
                        unique: false,
                        references: None,
                    })
                    .collect(),
                aliases: Vec::new(),
                primary_key: Vec::new(),
                unique_keys: Vec::new(),
                foreign_keys: Vec::new(),
            }
        })
        .collect();
    Some(ValidationSchema { tables, strict: None })
}
