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
