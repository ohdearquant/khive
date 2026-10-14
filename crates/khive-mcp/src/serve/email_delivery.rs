//! Typed inbound request assembly and required original-byte publication.

use base64::Engine as _;
use serde_json::json;

/// Build the internal request consumed by the actual poll loop. Verified external
/// routing is supplied once; an unverified transport header cannot overwrite it.
#[cfg(feature = "channel-email")]
pub(super) fn email_ingest_params(
    namespace: &str,
    (kind, slug): (&str, &str),
    default_actor: &str,
    env: &khive_channel::ChannelEnvelope,
    metadata: std::collections::HashMap<String, String>,
) -> serde_json::Value {
    let mut metadata: serde_json::Map<String, serde_json::Value> = metadata
        .into_iter()
        .map(|(key, value)| (key, serde_json::Value::String(value)))
        .collect();
    // Screened quarantines carry the channel's same rule-ID list without a delivered route.
    if kind == "email" {
        if let Some(labels) = metadata
            .get("labels")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        {
            metadata.insert("labels".into(), json!(labels));
        }
    }
    let delivery = env.inbound_delivery.as_ref();
    if let Some(delivery) = delivery {
        metadata.insert("labels".into(), json!(delivery.labels));
        metadata.insert(
            "screening_body_withheld".into(),
            json!(delivery.body_withheld),
        );
    }
    json!({
        "namespace": namespace,
        "from": env.from,
        "to": env.to,
        "content": env.content,
        "subject": env.subject,
        "channel_kind": kind,
        "channel_slug": slug,
        "external_id": env.external_id,
        "legacy_external_id": env.legacy_external_id,
        "sent_at": env.sent_at.as_ref().map(|ts| ts.to_rfc3339()),
        "correlation_external_id": if delivery.is_some() { None } else { env.correlation_external_id.as_deref() },
        "default_inbound_actor": delivery.map(|delivery| delivery.actor.as_str()).unwrap_or(default_actor),
        "thread_id": delivery.and_then(|delivery| delivery.thread_id).map(|id| id.to_string()),
        "reply_opt_out": delivery.is_some_and(|delivery| delivery.reply_opt_out),
        "pending_owner_candidates": delivery.and_then(|delivery| delivery.pending_owner_candidates.as_ref()),
        "wire_message_id": env.wire_message_id,
        "wire_references": env.wire_references,
        "metadata": metadata,
    })
}

/// A delivered placeholder always owns its byte-exact original. Quarantine's
/// retention count cannot authorize a delivered message without that owner.
#[cfg(feature = "channel-email")]
pub(super) async fn retain_withheld_original(
    registry: &khive_runtime::VerbRegistry,
    env: &khive_channel::ChannelEnvelope,
) -> Result<String, khive_runtime::RuntimeError> {
    let replay = env.quarantine_replay.as_ref().ok_or_else(|| {
        khive_runtime::RuntimeError::Unconfigured(
            "withheld email has no original replay context".into(),
        )
    })?;
    let result = registry
        .dispatch(
            "blob.put",
            json!({
                "bytes": base64::engine::general_purpose::STANDARD.encode(&replay.bytes),
            }),
        )
        .await?;
    result
        .get("content_ref")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            khive_runtime::RuntimeError::Internal(
                "blob.put returned no original content reference".into(),
            )
        })
}
