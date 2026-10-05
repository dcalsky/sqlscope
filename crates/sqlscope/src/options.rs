use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use polyglot_sql::DialectType;

use crate::error::Error;

/// A SQL dialect understood by the parser and generator.
///
/// Parse one from its name with [`str::parse`] (`"trino"`, `"postgres"`,
/// `"mysql"`, `"starrocks"`, `"spark"`, ...). The default is Trino.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Dialect(DialectType);

impl Dialect {
    pub const TRINO: Dialect = Dialect(DialectType::Trino);
    pub const PRESTO: Dialect = Dialect(DialectType::Presto);
    pub const POSTGRES: Dialect = Dialect(DialectType::PostgreSQL);
    pub const MYSQL: Dialect = Dialect(DialectType::MySQL);
    pub const STARROCKS: Dialect = Dialect(DialectType::StarRocks);
    pub const DORIS: Dialect = Dialect(DialectType::Doris);
    pub const HIVE: Dialect = Dialect(DialectType::Hive);
    pub const SPARK: Dialect = Dialect(DialectType::Spark);
    pub const DATABRICKS: Dialect = Dialect(DialectType::Databricks);
    pub const SNOWFLAKE: Dialect = Dialect(DialectType::Snowflake);
    pub const BIGQUERY: Dialect = Dialect(DialectType::BigQuery);
    pub const DUCKDB: Dialect = Dialect(DialectType::DuckDB);
    pub const ORACLE: Dialect = Dialect(DialectType::Oracle);
    pub const TSQL: Dialect = Dialect(DialectType::TSQL);
    pub const CLICKHOUSE: Dialect = Dialect(DialectType::ClickHouse);
    pub const SQLITE: Dialect = Dialect(DialectType::SQLite);

    pub(crate) fn polyglot(self) -> DialectType {
        self.0
    }
}

impl Default for Dialect {
    fn default() -> Self {
        Dialect::TRINO
    }
}

impl FromStr for Dialect {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self, Error> {
        let normalized = name.trim().to_ascii_lowercase();
        let lookup = match normalized.as_str() {
            "" => return Err(Error::invalid("dialect must not be empty")),
            "pg" => "postgresql",
            other => other,
        };
        DialectType::from_str(lookup)
            .map(Dialect)
            .map_err(|_| Error::invalid(format!("unknown dialect {name:?}")))
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Options shared by every sqlscope operation.
///
/// Every operation reads the [dialect](Options::dialect). The remaining
/// settings are read only by the operations that document them and are
/// ignored elsewhere:
///
/// | Setting | Read by |
/// | --- | --- |
/// | [`schema`](Options::schema) | [`column_origins`](crate::column_origins), [`output_columns`](crate::output_columns), [`referenced_columns`](crate::referenced_columns), [`column_usages`](crate::column_usages) |
/// | [`table_names`](Options::table_names), [`table_patterns`](Options::table_patterns), [`default_db`](Options::default_db) | [`apply_row_filter`](crate::apply_row_filter) |
/// | [`strip_catalogs`](Options::strip_catalogs) | [`rewrite_tables`](crate::rewrite_tables) |
///
/// ```
/// use sqlscope::{Dialect, Options};
///
/// let options = Options::new()
///     .dialect(Dialect::POSTGRES)
///     .schema([("orders", ["id", "status"])]);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub(crate) dialect: Dialect,
    pub(crate) schema: BTreeMap<String, Vec<String>>,
    pub(crate) table_names: Option<Vec<String>>,
    pub(crate) table_patterns: Vec<String>,
    pub(crate) default_db: Option<String>,
    pub(crate) strip_catalogs: Vec<String>,
}

impl Options {
    /// Options with the default (Trino) dialect and no other settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Selects the SQL dialect used to parse input and generate output.
    pub fn dialect(mut self, dialect: Dialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Supplies table schemas: a map from table name to its ordered column
    /// names.
    ///
    /// Keys may be bare (`orders`), schema-qualified (`sales.orders`) or
    /// catalog-qualified (`hive.sales.orders`); a qualified query reference
    /// also matches a key that is a dot-boundary suffix of it, and vice versa,
    /// when that match is unique. The schema expands wildcards and attributes
    /// unqualified columns. A table listed with no columns is known to have
    /// zero columns; a table that is absent is unknown.
    pub fn schema<K, C, S>(mut self, schema: impl IntoIterator<Item = (K, C)>) -> Self
    where
        K: Into<String>,
        C: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for (table, columns) in schema {
            self.schema
                .insert(table.into(), columns.into_iter().map(Into::into).collect());
        }
        self
    }

    /// Restricts [`apply_row_filter`](crate::apply_row_filter) to the named
    /// tables. Names may be bare (`orders`), schema-qualified (`sales.orders`)
    /// or catalog-qualified (`iceberg.sales.orders`) and match
    /// case-insensitively. Composes additively with
    /// [`table_patterns`](Options::table_patterns).
    pub fn table_names<S: Into<String>>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.table_names
            .get_or_insert_with(Vec::new)
            .extend(names.into_iter().map(Into::into));
        self
    }

    /// Restricts [`apply_row_filter`](crate::apply_row_filter) to tables whose
    /// bare or fully qualified name (as written) matches any of these regular
    /// expressions.
    pub fn table_patterns<S: Into<String>>(mut self, patterns: impl IntoIterator<Item = S>) -> Self {
        self.table_patterns.extend(patterns.into_iter().map(Into::into));
        self
    }

    /// The schema used to resolve unqualified table references against
    /// schema-qualified [`table_names`](Options::table_names), typically the
    /// session's current database.
    pub fn default_db(mut self, db: impl Into<String>) -> Self {
        self.default_db = Some(db.into());
        self
    }

    /// Catalogs that [`rewrite_tables`](crate::rewrite_tables) treats as
    /// transparent while matching: with `strip_catalogs(["hive"])`,
    /// `hive.sales.orders` matches the match key `sales.orders`.
    pub fn strip_catalogs<S: Into<String>>(mut self, catalogs: impl IntoIterator<Item = S>) -> Self {
        self.strip_catalogs.extend(catalogs.into_iter().map(Into::into));
        self
    }
}
