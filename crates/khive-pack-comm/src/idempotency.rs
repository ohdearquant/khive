//! Request identity and intact-pair reconciliation for caller-keyed messages.

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::Note;
use khive_types::{Details, KhiveError};
use serde_json::{json, Value};
use uuid::Uuid;

pub(crate) struct MessageIdentity {
    pub key: String,
    pub request: Value,
}

impl MessageIdentity {
    pub fn new(
        key: Option<&str>,
        request: impl FnOnce() -> Value,
    ) -> Result<Option<Self>, RuntimeError> {
        key.map(|key| {
            khive_runtime::keyed_memory::validate_memory_key(key)?;
            Ok(Self {
                key: key.to_owned(),
                request: request(),
            })
        })
        .transpose()
    }

    pub fn physical_key(&self, token: &NamespaceToken) -> String {
        format!(
            "comm-v1:{}",
            json!([token.namespace().as_str(), token.actor().id, self.key])
        )
    }

    fn conflict(&self, holder: Uuid) -> RuntimeError {
        KhiveError::conflict("message key is held by a different request or an incomplete pair")
            .with_details(Details::new_owned([
                ("reason", "key_conflict".into()),
                ("key", self.key.clone()),
                ("existing_id", holder.to_string()),
            ]))
            .into()
    }

    pub async fn replay(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        holder: Uuid,
    ) -> Result<Note, RuntimeError> {
        let store = runtime.notes(token)?;
        let first = store
            .get_note(holder)
            .await?
            .ok_or_else(|| self.conflict(holder))?;
        let recipient = first
            .properties
            .as_ref()
            .and_then(|p| p.get("inbound_ref"))
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<Uuid>().ok())
            .filter(|id| *id != holder)
            .ok_or_else(|| self.conflict(holder))?;
        // Re-read both together; the SQLite implementation hydrates this pair
        // with one SELECT rather than accepting an earlier outbound snapshot.
        let pair = store.get_notes_batch(&[holder, recipient]).await?;
        let outbound = pair
            .iter()
            .find(|note| note.id == holder)
            .ok_or_else(|| self.conflict(holder))?;
        let inbound = pair
            .iter()
            .find(|note| note.id == recipient)
            .ok_or_else(|| self.conflict(holder))?;
        let out = outbound
            .properties
            .as_ref()
            .ok_or_else(|| self.conflict(holder))?;
        let inc = inbound
            .properties
            .as_ref()
            .ok_or_else(|| self.conflict(holder))?;
        let physical = self.physical_key(token);
        let thread = self.request["thread_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| holder.to_string());
        let expected_tags = &self.request["tags"];
        let empty_tags = json!([]);
        let valid = pair.len() == 2
            && outbound.key.as_deref() == Some(physical.as_str())
            && inbound.key.is_none()
            && out["direction"] == "outbound"
            && inc["direction"] == "inbound"
            && out["inbound_ref"] == recipient.to_string()
            && inc["outbound_ref"] == holder.to_string()
            && out["idempotency_request"] == self.request
            && out["sent_at"].is_string()
            && out["sent_at"] == inc["sent_at"]
            && [outbound, inbound].into_iter().all(|note| {
                let Some(props) = note.properties.as_ref() else {
                    return false;
                };
                note.namespace == token.namespace().as_str()
                    && note.kind == "message"
                    && note.deleted_at.is_none()
                    && Some(note.content.as_str()) == self.request["content"].as_str()
                    && note.name.as_deref() == self.request["subject"].as_str()
                    && props["from"] == token.namespace().as_str()
                    && props["to"] == token.namespace().as_str()
                    && props["from_actor"] == token.actor().id
                    && props["to_actor"] == self.request["to"]
                    && props["subject"] == self.request["subject"]
                    && props["thread_id"] == thread
                    && props["idempotency_key"] == self.key
                    && props.get("tags").unwrap_or(&empty_tags) == expected_tags
            });
        if !valid {
            return Err(self.conflict(holder));
        }
        Ok(outbound.clone())
    }

    pub fn annotate_response(&self, response: &mut Value, outbound: &Note, replayed: bool) {
        let props = outbound.properties.as_ref().expect("keyed pair properties");
        response["idempotency_key"] = json!(self.key);
        response["replayed"] = json!(replayed);
        response["recipient_id"] = props["inbound_ref"].clone();
        for field in ["sent_at", "thread_id", "subject"] {
            response[field] = props[field].clone();
        }
        response["to"] = props["to_actor"].clone();
    }
}
