//! Generic role-keyed record attachments (ADR-121 rollout step 2).

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_runtime::{NamespaceToken, RuntimeError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::common::{deser, resolve_uuid_unfiltered};
use crate::KgPack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachParams {
    id: String,
    role: String,
    bytes: String,
    media_type: Option<String>,
}

fn decode_attachment_bytes(encoded: &str, max_bytes: u64) -> Result<Vec<u8>, RuntimeError> {
    // Match the portable inline blob.put envelope before allocating decoded bytes.
    let max_encoded_len = max_bytes.saturating_mul(4) / 3 + 4;
    if encoded.len() as u64 > max_encoded_len {
        return Err(RuntimeError::InvalidInput(format!(
            "attach base64 input is {} chars, exceeding the {max_bytes}-byte ceiling",
            encoded.len()
        )));
    }
    let bytes = BASE64.decode(encoded).map_err(|error| {
        RuntimeError::InvalidInput(format!("attach bytes must be valid base64: {error}"))
    })?;
    if bytes.len() as u64 > max_bytes {
        return Err(RuntimeError::InvalidInput(format!(
            "attach input is {} bytes, exceeding the {max_bytes}-byte maximum",
            bytes.len()
        )));
    }
    Ok(bytes)
}

impl KgPack {
    pub(crate) async fn handle_attach(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: AttachParams = deser(params)?;
        let id = resolve_uuid_unfiltered(&p.id, &self.runtime, token).await?;
        let bytes = decode_attachment_bytes(&p.bytes, khive_storage::MAX_BLOB_WHOLE_BYTES)?;
        let (attachment, replaced) = self
            .runtime
            .publish_record_attachment(token, id, p.role, bytes, p.media_type)
            .await?;
        Ok(json!({
            "id": id,
            "role": attachment.role,
            "content_ref": attachment.content_ref,
            "size": attachment.size_bytes,
            "media_type": attachment.media_type,
            "replaced": replaced,
        }))
    }

    pub(super) async fn get_with_attachments(
        &self,
        id: Uuid,
        mut record: Value,
        hydrate: &[String],
    ) -> Result<Value, RuntimeError> {
        let core = self.runtime.core();
        let attachments = core.attachments()?.list_attachments(id).await?;
        for role in hydrate {
            if !attachments
                .iter()
                .any(|attachment| &attachment.role == role)
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown attachment role {role:?} on record {id}"
                )));
            }
        }
        if attachments.is_empty() {
            return Ok(record);
        }
        let mut metadata = serde_json::Map::new();
        for attachment in attachments {
            let mut entry = json!({
                "role": attachment.role,
                "media_type": attachment.media_type,
                "size": attachment.size_bytes,
                "content_ref": attachment.content_ref,
            });
            if hydrate.contains(&attachment.role) {
                let size = match attachment.size_bytes {
                    Some(size) => size,
                    None => {
                        let store = core.require_blob_store()?;
                        khive_storage::await_request_read_phase(
                            "attachment_blob_size",
                            store.size(&attachment.content_ref),
                        )
                        .await??
                        .ok_or_else(|| {
                            RuntimeError::NotFound(format!(
                                "attachment role {:?} on record {id} has no stored blob {}",
                                attachment.role, attachment.content_ref
                            ))
                        })?
                    }
                };
                let bytes = core
                    .require_blob_hydrator()?
                    .hydrate_verified(&attachment.content_ref, size)
                    .await?;
                if bytes.bytes().len() as u64 != size {
                    return Err(RuntimeError::InvalidInput(format!(
                        "attachment role {:?} on record {id} has inconsistent stored size",
                        attachment.role
                    )));
                }
                entry["bytes"] = json!(BASE64.encode(bytes.bytes()));
            }
            metadata.insert(attachment.role, entry);
        }
        record["attachments"] = Value::Object(metadata);
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::decode_attachment_bytes;

    #[test]
    fn inline_attachment_decode_enforces_preallocation_and_exact_decoded_limits() {
        // Invalid alphabet above the envelope must refuse on length before decoding.
        let error = decode_attachment_bytes("%%%%%%%%%%%%%", 6).unwrap_err();
        assert!(error.to_string().contains("6-byte ceiling"), "{error}");
        let error = decode_attachment_bytes("%", 3).unwrap_err();
        assert!(error.to_string().contains("valid base64"), "{error}");
        // Four decoded bytes fit the permissive encoded envelope for a three-byte cap.
        let error = decode_attachment_bytes("YWJjZA==", 3).unwrap_err();
        assert!(error.to_string().contains("3-byte maximum"), "{error}");
        assert_eq!(decode_attachment_bytes("YWJj", 3).unwrap(), b"abc");
        assert_eq!(decode_attachment_bytes("", 0).unwrap(), b"");
    }
}
