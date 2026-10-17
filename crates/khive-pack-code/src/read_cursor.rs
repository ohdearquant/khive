//! Short-lived, map-registration-bound pagination state (ADR-139).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use khive_runtime::RuntimeError;
use serde::{Deserialize, Serialize};

use crate::read_common::invalid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    pub map: String,
    pub generation: String,
    pub query: Option<String>,
    pub kind: Option<String>,
    pub project_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "verb", deny_unknown_fields)]
pub(super) enum Position {
    List { name: String, id: String },
    Search { score: u8, id: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    pub binding: Binding,
    pub position: Position,
}

impl Cursor {
    pub fn decode(token: &str, search: bool) -> Result<Self, RuntimeError> {
        let malformed = || invalid("cursor must be a valid code map pagination token");
        let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| malformed())?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
        match &cursor.position {
            Position::List { id, .. } if !search && !id.trim().is_empty() => (),
            Position::Search { score, id }
                if search && (1..=3).contains(score) && !id.trim().is_empty() =>
            {
                ()
            }
            _ => return Err(malformed()),
        }
        Ok(cursor)
    }

    pub fn validate(&self, binding: &Binding) -> Result<(), RuntimeError> {
        if self.binding != *binding {
            return Err(invalid(
                "cursor is stale for this map registration or request",
            ));
        }
        Ok(())
    }

    pub fn encode(self) -> Result<String, RuntimeError> {
        serde_json::to_vec(&self)
            .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
            .map_err(|_| RuntimeError::Internal("code map cursor encoding failed".into()))
    }
}
