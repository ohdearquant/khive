use super::*;
use khive_channel::{Channel, ChannelEnvelope, ChannelRegistry};
use khive_storage::note::Note;

pub(super) enum OutboxPolicy<'a> {
    #[cfg(feature = "channel-email")]
    Email {
        mailbox: &'a str,
        domain: &'a str,
        allowlist: &'a [String],
    },
    #[cfg(feature = "channel-telegram")]
    Telegram(std::marker::PhantomData<&'a ()>),
}

// The borrowed path preserves the existing single-adapter entry points.
// Production loops use Registered, including for the current single-adapter configuration.
pub(super) enum OutboxChannels<'a> {
    #[cfg(all(test, feature = "channel-email"))]
    Single(&'a dyn Channel),
    Registered {
        registry: &'a ChannelRegistry,
        slug: &'a str,
    },
}

enum SelectedChannel<'a> {
    #[cfg(all(test, feature = "channel-email"))]
    Borrowed(&'a dyn Channel),
    Owned(Arc<dyn Channel>, std::marker::PhantomData<&'a ()>),
}
impl SelectedChannel<'_> {
    fn channel(&self) -> &dyn Channel {
        match self {
            #[cfg(all(test, feature = "channel-email"))]
            Self::Borrowed(c) => *c,
            Self::Owned(c, _) => c.as_ref(),
        }
    }
}

impl OutboxChannels<'_> {
    fn scan_scope(&self, kind: &str) -> (String, bool) {
        match self {
            #[cfg(all(test, feature = "channel-email"))]
            Self::Single(channel) => (channel.slug(), true),
            Self::Registered { registry, slug } => (
                (*slug).to_string(),
                registry
                    .iter()
                    .filter(|(registered, _, _)| *registered == kind)
                    .take(2)
                    .count()
                    == 1,
            ),
        }
    }

    fn select(
        &self,
        kind: &str,
        properties: &serde_json::Map<String, serde_json::Value>,
        warned: &mut std::collections::HashSet<String>,
    ) -> Option<SelectedChannel<'_>> {
        let requested = properties.get("channel_slug");
        let slug = match requested {
            Some(serde_json::Value::String(slug)) => Some(slug.as_str()),
            None => None,
            Some(_) => {
                if warned.insert("invalid-slug".into()) {
                    tracing::warn!(
                        channel = kind,
                        "outbox loop: invalid channel slug; holding pending rows"
                    );
                }
                return None;
            }
        };
        match self {
            #[cfg(all(test, feature = "channel-email"))]
            Self::Single(channel) => {
                if slug.is_none_or(|slug| slug == channel.slug()) {
                    Some(SelectedChannel::Borrowed(*channel))
                } else {
                    if warned.insert(format!("unknown:{slug:?}")) {
                        tracing::warn!(
                            channel = kind,
                            channel_slug = slug,
                            "outbox loop: unconfigured channel slug; holding pending rows"
                        );
                    }
                    None
                }
            }
            Self::Registered {
                registry,
                slug: pass_slug,
            } => {
                let resolved_slug = match slug {
                    Some(slug) => slug.to_string(),
                    None => {
                        let mut channels = registry.iter().filter(|(k, _, _)| *k == kind);
                        let first = channels.next();
                        if first.is_none() || channels.next().is_some() {
                            if warned.insert("ambiguous-legacy".into()) {
                                tracing::warn!(channel = kind, "outbox loop: legacy row requires exactly one channel; holding pending rows");
                            }
                            return None;
                        }
                        first.unwrap().1.to_string()
                    }
                };
                let channel = registry.get_by_slug(kind, &resolved_slug);
                let Some(channel) = channel else {
                    if warned.insert(format!("unknown:{resolved_slug}")) {
                        tracing::warn!(channel = kind, channel_slug = %resolved_slug, "outbox loop: unconfigured channel slug; holding pending rows");
                    }
                    return None;
                };
                if resolved_slug != *pass_slug {
                    return None;
                }
                Some(SelectedChannel::Owned(channel, std::marker::PhantomData))
            }
        }
    }
}

struct Prepared {
    envelope: ChannelEnvelope,
    external_id: Option<String>,
    recipient: String,
}

impl OutboxPolicy<'_> {
    fn kind(&self) -> &'static str {
        match self {
            #[cfg(feature = "channel-email")]
            Self::Email { .. } => "email",
            #[cfg(feature = "channel-telegram")]
            Self::Telegram(_) => "telegram",
        }
    }
    fn prefix(&self) -> &'static str {
        match self {
            #[cfg(feature = "channel-email")]
            Self::Email { .. } => "email:",
            #[cfg(feature = "channel-telegram")]
            Self::Telegram(_) => "telegram:",
        }
    }
    async fn prepare(
        &self,
        runtime: &KhiveRuntime,
        token: &khive_runtime::NamespaceToken,
        note: &Note,
    ) -> Option<Prepared> {
        match self {
            #[cfg(feature = "channel-email")]
            Self::Email {
                mailbox,
                domain,
                allowlist,
            } => prepare_email(runtime, token, note, mailbox, domain, allowlist).await,
            #[cfg(feature = "channel-telegram")]
            Self::Telegram(_) => {
                let _ = (runtime, token);
                let to = note.properties.as_ref()?.get("to_actor")?.as_str()?;
                Some(Prepared {
                    envelope: ChannelEnvelope::new("telegram:bot", to, note.content.clone()),
                    external_id: None,
                    recipient: to.to_string(),
                })
            }
        }
    }
    fn scan_error(&self, error: &khive_runtime::RuntimeError, authorization: bool) {
        match (self.kind(), authorization) {
            ("email", true) => {
                tracing::warn!(target: "khive_mcp::serve", error = %error, "outbox loop: namespace authorization failed")
            }
            ("email", false) => {
                tracing::warn!(target: "khive_mcp::serve", error = %error, "outbox loop: outbox scan failed")
            }
            (_, true) => {
                tracing::warn!(target: "khive_mcp::serve", error = %error, "telegram outbox loop: namespace authorization failed")
            }
            (_, false) => {
                tracing::warn!(target: "khive_mcp::serve", error = %error, "telegram outbox loop: outbox scan failed")
            }
        }
    }
    fn delivered(
        &self,
        id: uuid::Uuid,
        recipient: &str,
        message_id: Option<&str>,
        result: khive_runtime::RuntimeResult<Note>,
    ) {
        match (self.kind(), result) {
            ("email", Ok(_)) => {
                tracing::info!(target: "khive_mcp::serve", note_id = %id, recipient = %recipient, message_id = %message_id.unwrap_or_default(), "outbox loop: delivered")
            }
            ("email", Err(error)) => {
                tracing::warn!(target: "khive_mcp::serve", note_id = %id, error = %error, "outbox loop: failed to set delivered_at (AT-LEAST-ONCE: will retry)")
            }
            (_, Ok(_)) => {
                tracing::info!(target: "khive_mcp::serve", note_id = %id, "telegram outbox loop: delivered")
            }
            (_, Err(error)) => {
                tracing::warn!(target: "khive_mcp::serve", note_id = %id, error = %error, "telegram outbox loop: failed to set delivered_at (AT-LEAST-ONCE: will retry)")
            }
        }
    }
    fn failed(
        &self,
        id: uuid::Uuid,
        recipient: &str,
        error: &khive_channel::ChannelError,
        result: khive_runtime::RuntimeResult<Note>,
    ) {
        match (self.kind(), result) {
            ("email", Ok(_)) => {
                tracing::warn!(target: "khive_mcp::serve", note_id = %id, recipient = %recipient, error = %error, classification = ?error.delivery_failure_class(), "outbox loop: send failure recorded")
            }
            ("email", Err(mark_error)) => {
                tracing::warn!(target: "khive_mcp::serve", note_id = %id, recipient = %recipient, error = %error, mark_error = %mark_error, "outbox loop: send failed and retry state could not be recorded")
            }
            (_, Ok(_)) => {
                tracing::warn!(target: "khive_mcp::serve", note_id = %id, error = %error, classification = ?error.delivery_failure_class(), "telegram outbox loop: send failure recorded")
            }
            (_, Err(mark_error)) => {
                tracing::warn!(target: "khive_mcp::serve", note_id = %id, error = %error, mark_error = %mark_error, "telegram outbox loop: send failed and retry state could not be recorded")
            }
        }
    }
}

/// Row carries `channel_slug`: only the channel registered as exactly `(kind, slug)` may take it.
/// If no configured channel has that slug, the row stays pending (delivery state untouched,
/// never failed). No credential is touched.
/// Row carries no `channel_slug`: it is taken only when EXACTLY ONE channel of that kind is
/// configured. With two or more same-kind channels configured, the row stays pending.
/// It is never routed by guess, order, default or first match.
/// The SQL scan applies these constraints before its page bound; an ineligible
/// row cannot keep a later eligible row out of a finite delivery pass.
/// No producer changes are required; selection precedes any external-id claim or send.
pub(super) async fn outbox_once(
    channels: OutboxChannels<'_>,
    policy: OutboxPolicy<'_>,
    runtime: &KhiveRuntime,
    namespace: &khive_runtime::Namespace,
    cancellation: &tokio_util::sync::CancellationToken,
    pause_until: &mut Option<tokio::time::Instant>,
) -> Result<(), crate::components::ComponentError> {
    use crate::components::ComponentError;
    if pause_until
        .as_ref()
        .is_some_and(|deadline| tokio::time::Instant::now() < *deadline)
    {
        return Ok(());
    }
    *pause_until = None;
    let token = runtime.authorize(namespace.clone()).map_err(|error| {
        policy.scan_error(&error, true);
        ComponentError::Permanent(error.to_string())
    })?;
    let (channel_slug, include_legacy) = channels.scan_scope(policy.kind());
    let notes = runtime
        .list_undelivered_outbound_messages_for_channel(
            &token,
            policy.prefix(),
            &channel_slug,
            include_legacy,
            200,
        )
        .await
        .map_err(|error| {
            policy.scan_error(&error, false);
            ComponentError::Retryable(error.to_string())
        })?;
    let mut warned = std::collections::HashSet::new();
    for note in notes {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let Some(props) = note
            .properties
            .as_ref()
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        if props.get("direction").and_then(serde_json::Value::as_str) != Some("outbound")
            || note_already_delivered(props)
        {
            continue;
        }
        if !props
            .get("to_actor")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|actor| actor.starts_with(policy.prefix()))
        {
            continue;
        }
        let Some(channel) = channels.select(policy.kind(), props, &mut warned) else {
            continue;
        };
        let Some(prepared) = policy.prepare(runtime, &token, &note).await else {
            continue;
        };
        match channel.channel().send(prepared.envelope).await {
            Ok(()) => {
                let result = runtime
                    .mark_outbound_message_delivered(
                        &token,
                        note.id,
                        chrono::Utc::now().to_rfc3339(),
                        prepared.external_id.clone(),
                    )
                    .await;
                policy.delivered(
                    note.id,
                    &prepared.recipient,
                    prepared.external_id.as_deref(),
                    result,
                );
            }
            Err(
                khive_channel::ChannelError::Auth(error)
                | khive_channel::ChannelError::Config(error),
            ) => return Err(ComponentError::Permanent(error)),
            Err(khive_channel::ChannelError::RetryableAuth(error)) => {
                return Err(ComponentError::Retryable(error))
            }
            Err(error) => {
                let rate_limit = match &error {
                    khive_channel::ChannelError::RateLimited { retry_after, .. } => {
                        *pause_until = Some(tokio::time::Instant::now() + *retry_after);
                        true
                    }
                    _ => false,
                };
                let result = record_outbound_send_failure(runtime, &token, note.id, &error).await;
                policy.failed(note.id, &prepared.recipient, &error, result);
                if rate_limit {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

#[cfg(feature = "channel-email")]
async fn prepare_email(
    runtime: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    note: &Note,
    mailbox: &str,
    domain: &str,
    allowlist: &[String],
) -> Option<Prepared> {
    use chrono::Utc;
    let props = note.properties.as_ref()?.as_object()?;
    let note_id = note.id.to_string();
    let recipient = props
        .get("to_actor")?
        .as_str()?
        .strip_prefix("email:")?
        .to_string();
    if !allowlist.is_empty() && !allowlist.contains(&recipient) {
        // ADR-122 §2: an allowlist rejection is a PERMANENT failure and
        // must be recorded — skipping with only a log line leaves the row
        // pending forever while the sender saw `ok: true`.
        let failed_at = Utc::now().to_rfc3339();
        let last_error = format!("recipient {recipient} not in outbound allowlist");
        let mark_result = match uuid::Uuid::parse_str(&note_id) {
            Ok(uuid) => runtime
                .mark_outbound_message_failed(token, uuid, failed_at, last_error.clone())
                .await
                .map(|_| ()),
            Err(error) => Err(khive_runtime::RuntimeError::InvalidInput(format!(
                "note id {note_id} is not a valid UUID: {error}"
            ))),
        };
        match mark_result {
            Ok(_) => tracing::warn!(target: "khive_mcp::serve",
                note_id = %note_id,
                recipient = %recipient,
                "outbox loop: recipient not in allowlist; recorded permanent failure"
            ),
            Err(error) => tracing::warn!(target: "khive_mcp::serve",
                note_id = %note_id,
                recipient = %recipient,
                error = %error,
                "outbox loop: recipient not in allowlist; failed to record failure (will re-encounter)"
            ),
        }
        return None;
    }

    let subject = props
        .get("subject")
        .and_then(|value| value.as_str())
        .unwrap_or("(no subject)")
        .to_string();
    let content = note.content.clone();
    let thread_id = props
        .get("thread_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let in_reply_to = props
        .get("in_reply_to_message_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let references = props
        .get("references_chain")
        .and_then(|value| value.as_str())
        .map(str::to_string);

    // Mint-before-send through the comm-routed runtime's owner-only path.
    // Generic `update` correctly refuses caller patches to `external_id`.
    let message_id = match props.get("external_id").and_then(|value| value.as_str()) {
        Some(external_id) if !external_id.is_empty() => external_id.to_string(),
        _ => {
            let message_id = format!("<{note_id}@{domain}>");
            let claim_result = match uuid::Uuid::parse_str(&note_id) {
                Ok(uuid) => {
                    runtime
                        .claim_outbound_message_external_id(token, uuid, message_id.clone())
                        .await
                }
                Err(error) => Err(khive_runtime::RuntimeError::InvalidInput(format!(
                    "note id {note_id} is not a valid UUID: {error}"
                ))),
            };
            if let Err(error) = claim_result {
                let mark_result = match uuid::Uuid::parse_str(&note_id) {
                    Ok(uuid) => record_outbound_claim_failure(runtime, token, uuid, &error).await,
                    Err(parse_error) => Err(khive_runtime::RuntimeError::InvalidInput(format!(
                        "note id {note_id} is not a valid UUID: {parse_error}"
                    ))),
                };
                match mark_result {
                    Ok(note) => tracing::warn!(target: "khive_mcp::serve",
                        note_id = %note_id,
                        error = %error,
                        permanent = outbound_claim_failure_is_permanent(&error),
                        delivery = ?note.properties.as_ref().and_then(|p| p.get("delivery")).and_then(|v| v.as_str()),
                        "outbox loop: claim failure handled; existing claim or terminal state preserved"
                    ),
                    Err(mark_error) => tracing::warn!(target: "khive_mcp::serve",
                        note_id = %note_id,
                        error = %error,
                        mark_error = %mark_error,
                        "outbox loop: claim failed and failure state could not be recorded"
                    ),
                }
                return None;
            }
            message_id
        }
    };

    let mut envelope = ChannelEnvelope::new(
        format!("email:{mailbox}"),
        format!("email:{recipient}"),
        content,
    )
    .with_subject(subject)
    .with_message_id(message_id.clone());
    if let Some(thread_id) = thread_id {
        envelope = envelope.with_correlation(thread_id);
    }
    if let Some(in_reply_to) = in_reply_to {
        envelope = envelope.with_in_reply_to(in_reply_to);
    }
    if let Some(references) = references {
        envelope = envelope.with_references(references);
    }

    Some(Prepared {
        envelope,
        external_id: Some(message_id),
        recipient,
    })
}

/// A loop with no selectable adapter must fail before emitting liveness.
pub(super) fn validate_loop_channel(
    channel: &dyn Channel,
    expected_kind: &str,
) -> Result<(), crate::components::ComponentError> {
    if channel.kind() != expected_kind {
        return Err(crate::components::ComponentError::Permanent(format!(
            "outbox adapter kind {:?} does not match configured policy {:?}",
            channel.kind(),
            expected_kind
        )));
    }
    Ok(())
}
