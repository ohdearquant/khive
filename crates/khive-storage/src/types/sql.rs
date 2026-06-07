//! SQL-related shared types: values, statements, rows, and transaction options.

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

impl SqlRow {
    /// Look up a column value by name, returning `None` if absent.
    pub fn get(&self, name: &str) -> Option<&SqlValue> {
        self.columns
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.value)
    }
}

/// Transaction isolation level hint for SQL backends that support it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SqlIsolation {
    Default,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// Options passed to a SQL transaction begin call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlTxOptions {
    pub read_only: bool,
    pub isolation: SqlIsolation,
    pub label: Option<String>,
}

impl Default for SqlTxOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            isolation: SqlIsolation::Default,
            label: None,
        }
    }
}
