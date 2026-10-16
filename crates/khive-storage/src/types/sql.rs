//! SQL-related shared types: values, statements, and rows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// A tagged SQL column value that can round-trip through serde and SQLite bindings.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
    Json(Value),
    Uuid(Uuid),
    Timestamp(DateTime<Utc>),
}

impl SqlValue {
    /// Bind optional text: `Some` becomes [`SqlValue::Text`], including the empty
    /// string, and `None` becomes [`SqlValue::Null`].
    pub fn from_opt_text(value: Option<&str>) -> SqlValue {
        match value {
            Some(text) => SqlValue::Text(text.to_owned()),
            None => SqlValue::Null,
        }
    }

    /// Bind an optional integer: `Some` becomes [`SqlValue::Integer`], including
    /// zero, and `None` becomes [`SqlValue::Null`].
    pub fn from_opt_i64(value: Option<i64>) -> SqlValue {
        match value {
            Some(number) => SqlValue::Integer(number),
            None => SqlValue::Null,
        }
    }
}

/// A parameterized SQL statement with optional diagnostic label.
///
/// `sql` is one SQLite statement, not a script. Backends must reject trailing
/// executable SQL (including a trailing transaction-control statement) before
/// executing the statement; use [`crate::SqlWriter::execute_batch`] for
/// multiple parameterized statements and [`crate::SqlWriter::execute_script`]
/// for raw scripts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlStatement {
    pub sql: String,
    pub params: Vec<SqlValue>,
    pub label: Option<String>,
}

impl SqlStatement {
    /// Create a statement without a diagnostic label.
    pub fn new(sql: impl Into<String>, params: Vec<SqlValue>) -> Self {
        Self {
            sql: sql.into(),
            params,
            label: None,
        }
    }

    /// Set or replace the diagnostic label for this statement.
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// A single named column in a SQL result row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlColumn {
    pub name: String,
    pub value: SqlValue,
}

/// A row of named columns returned by a raw SQL query.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlRow {
    pub columns: Vec<SqlColumn>,
}

/// A refused typed column read. `found` is the SQL variant name, or `None` for absence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqlColumnError {
    pub column: String,
    pub found: Option<&'static str>,
}

impl std::fmt::Display for SqlColumnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.found {
            Some(found) => write!(f, "SQL column {} has value {found}", self.column),
            None => write!(f, "SQL column {} is absent", self.column),
        }
    }
}

impl std::error::Error for SqlColumnError {}

impl SqlRow {
    /// Look up a column value by name, returning `None` if absent.
    pub fn get(&self, name: &str) -> Option<&SqlValue> {
        self.columns
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.value)
    }

    /// Read text, refusing NULL, absence, and other SQL variants.
    pub fn text(&self, name: &str) -> Result<&str, SqlColumnError> {
        match self.get(name) {
            Some(SqlValue::Text(value)) => Ok(value),
            found => Err(column_error(name, found)),
        }
    }

    /// Read an integer, refusing NULL, absence, and other SQL variants, including Float.
    pub fn i64(&self, name: &str) -> Result<i64, SqlColumnError> {
        match self.get(name) {
            Some(SqlValue::Integer(value)) => Ok(*value),
            found => Err(column_error(name, found)),
        }
    }

    /// Read nullable text, refusing an absent column or another SQL variant.
    pub fn opt_text(&self, name: &str) -> Result<Option<&str>, SqlColumnError> {
        match self.get(name) {
            Some(SqlValue::Null) => Ok(None),
            Some(SqlValue::Text(value)) => Ok(Some(value)),
            found => Err(column_error(name, found)),
        }
    }

    /// Read a nullable integer, refusing an absent column or another SQL variant.
    pub fn opt_i64(&self, name: &str) -> Result<Option<i64>, SqlColumnError> {
        match self.get(name) {
            Some(SqlValue::Null) => Ok(None),
            Some(SqlValue::Integer(value)) => Ok(Some(*value)),
            found => Err(column_error(name, found)),
        }
    }

    /// Read nullable text, treating absence as NULL while refusing other SQL variants.
    pub fn opt_text_or_absent(&self, name: &str) -> Result<Option<&str>, SqlColumnError> {
        match self.get(name) {
            None => Ok(None),
            _ => self.opt_text(name),
        }
    }

    /// Read a nullable integer, treating absence as NULL while refusing other SQL variants.
    pub fn opt_i64_or_absent(&self, name: &str) -> Result<Option<i64>, SqlColumnError> {
        match self.get(name) {
            None => Ok(None),
            _ => self.opt_i64(name),
        }
    }

    /// Read a native UUID or parse UUID text, refusing NULL, absence, invalid
    /// UUID text, and other SQL variants.
    pub fn uuid(&self, name: &str) -> Result<Uuid, SqlColumnError> {
        let found = self.get(name);
        match found {
            Some(SqlValue::Uuid(value)) => Ok(*value),
            Some(SqlValue::Text(value)) => {
                Uuid::parse_str(value).map_err(|_| column_error(name, found))
            }
            _ => Err(column_error(name, found)),
        }
    }

    /// Read a nullable UUID with the same accepted representations as [`Self::uuid`].
    /// An absent column, invalid UUID text, or another SQL variant is refused.
    pub fn opt_uuid(&self, name: &str) -> Result<Option<Uuid>, SqlColumnError> {
        match self.get(name) {
            Some(SqlValue::Null) => Ok(None),
            _ => self.uuid(name).map(Some),
        }
    }

    /// Read a Float unchanged or convert an Integer using Rust's `as f64`
    /// rounding. Non-finite Floats are preserved; NULL, absence, and other
    /// SQL variants (including numeric text) are refused.
    pub fn f64(&self, name: &str) -> Result<f64, SqlColumnError> {
        match self.get(name) {
            Some(SqlValue::Float(value)) => Ok(*value),
            Some(SqlValue::Integer(value)) => Ok(*value as f64),
            found => Err(column_error(name, found)),
        }
    }

    /// Read a nullable number using [`Self::f64`], refusing an absent column
    /// or another SQL variant.
    pub fn opt_f64(&self, name: &str) -> Result<Option<f64>, SqlColumnError> {
        match self.get(name) {
            Some(SqlValue::Null) => Ok(None),
            _ => self.f64(name).map(Some),
        }
    }

    /// Read text, treating absence, NULL, and every other SQL variant as `None`.
    pub fn text_or_none(&self, name: &str) -> Option<&str> {
        match self.get(name) {
            Some(SqlValue::Text(value)) => Some(value),
            _ => None,
        }
    }

    /// Read an integer, treating absence, NULL, and every other SQL variant
    /// (including Float) as `None`.
    pub fn i64_or_none(&self, name: &str) -> Option<i64> {
        match self.get(name) {
            Some(SqlValue::Integer(value)) => Some(*value),
            _ => None,
        }
    }
}

fn column_error(name: &str, found: Option<&SqlValue>) -> SqlColumnError {
    SqlColumnError {
        column: name.to_owned(),
        found: found.map(|value| match value {
            SqlValue::Null => "Null",
            SqlValue::Bool(_) => "Bool",
            SqlValue::Integer(_) => "Integer",
            SqlValue::Float(_) => "Float",
            SqlValue::Text(_) => "Text",
            SqlValue::Blob(_) => "Blob",
            SqlValue::Json(_) => "Json",
            SqlValue::Uuid(_) => "Uuid",
            SqlValue::Timestamp(_) => "Timestamp",
        }),
    }
}
