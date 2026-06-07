//! Handler for `memory.feedback` — explicit recall-domain feedback.

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::RuntimeError;

use crate::recall_feedback::on_explicit_feedback;
use crate::MemoryPack;

#[derive(Debug, Deserialize)]
struct FeedbackParams {
    target_id: String,
    signal: String,
}

impl MemoryPack {
    pub(crate) async fn handle_feedback(&self, params: Value) -> Result<Value, RuntimeError> {
        let p: FeedbackParams = serde_json::from_value(params).map_err(|e| {
            RuntimeError::InvalidInput(format!("memory.feedback: invalid params: {e}"))
        })?;

        let target_id = p.target_id.parse::<Uuid>().map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "memory.feedback: target_id {:?} is not a valid UUID",
                p.target_id
            ))
        })?;

        if let Ok(mut state) = self.recall_state.lock() {
            on_explicit_feedback(&mut state, target_id, &p.signal);
        }

        Ok(json!({ "ok": true, "target_id": p.target_id, "signal": p.signal }))
    }
}
