use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Per-request context — session, timing, transport source.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GateContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}
