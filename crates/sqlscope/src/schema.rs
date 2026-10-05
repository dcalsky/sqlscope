//! Lookups in the caller-supplied `table -> columns` schema.

use std::collections::BTreeMap;

use polyglot_sql::{SchemaColumn, SchemaTable, ValidationSchema};

use crate::ast::has_table_suffix;

pub(crate) type Schema = BTreeMap<String, Vec<String>>;

/// The schema key for `table`: an exact key, or the unique key that has
/// `table` as a dot-boundary suffix.
pub(crate) fn key_for<'s>(schema: &'s Schema, table: &str) -> Option<&'s String> {
    if table.is_empty() {
        return None;
    }
    if let Some((key, _)) = schema.get_key_value(table) {
        return Some(key);
    }
    let mut candidates = schema.keys().filter(|key| has_table_suffix(key, table));
    match (candidates.next(), candidates.next()) {
        (Some(key), None) => Some(key),
        _ => None,
    }
}

/// The columns of `table`, when its schema is known.
pub(crate) fn columns<'s>(schema: &'s Schema, table: &str) -> Option<&'s [String]> {
    key_for(schema, table).map(|key| schema[key].as_slice())
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
