//! The daemon's implementation of the email channel's external-email host.

use std::sync::Arc;

use async_trait::async_trait;
use khive_channel::ChannelError;
use khive_channel_email::{ExternalEmailHost, InboundSuppression, ThreadOwner};
use khive_runtime::{EmailMessageIdDomains, KhiveRuntime, Namespace, OutboundEmailPolicy};

/// The external-email services the email channel takes from its daemon: the boot-resolved
/// outbound policy and, when one is wired, the comm store (ADR-200 sections 5.4 and 7).
pub(super) struct CommExternalEmailHost {
    policy: Arc<OutboundEmailPolicy>,
    runtime: Option<KhiveRuntime>,
    namespace: Namespace,
    email_domains: Result<Option<EmailMessageIdDomains>, String>,
}

impl CommExternalEmailHost {
    #[cfg(test)]
    pub(super) fn new(policy: Arc<OutboundEmailPolicy>, runtime: Option<KhiveRuntime>) -> Self {
        Self::for_ingest(
            policy,
            runtime,
            Namespace::local(),
            EmailMessageIdDomains::from_env(),
        )
    }

    pub(super) fn for_ingest(
        policy: Arc<OutboundEmailPolicy>,
        runtime: Option<KhiveRuntime>,
        namespace: Namespace,
        email_domains: Result<Option<EmailMessageIdDomains>, String>,
    ) -> Self {
        Self {
            policy,
            runtime,
            namespace,
            email_domains,
        }
    }
}

#[async_trait]
impl ExternalEmailHost for CommExternalEmailHost {
    fn external_senders_configured(&self) -> bool {
        self.policy.has_external_senders()
    }

    fn verify_unsubscribe_token(&self, token: &str) -> Option<String> {
        self.policy.verify_unsubscribe_token(token)
    }

    async fn inbound_suppression(&self) -> Result<InboundSuppression, ChannelError> {
        let Some(runtime) = self.runtime.as_ref() else {
            return Ok(Arc::new(|_: &str| false));
        };
        let entries = khive_pack_comm::handlers::load_inbound_email_suppressions(runtime)
            .await
            .map_err(|error| {
                ChannelError::Transport(format!("inbound suppression read failed: {error}"))
            })?;
        Ok(Arc::new(move |sender: &str| {
            khive_runtime::inbound_email_is_suppressed(sender, &entries)
        }))
    }

    async fn record_mailto_unsubscribe(&self, recipient_key: &str) -> Result<bool, ChannelError> {
        let Some(runtime) = self.runtime.as_ref() else {
            tracing::warn!(
                "mailto unsubscribe: no comm store is wired to this channel; suppression entry not recorded"
            );
            return Err(ChannelError::Transport(
                "mailto unsubscribe: no comm store is wired to this channel".to_string(),
            ));
        };
        // Attribute the write to the serving actor, or to the channel's own identity when the
        // process has none configured, so a verified opt-out is never dropped for want of an actor.
        let actor = runtime
            .config()
            .actor_id
            .clone()
            .unwrap_or_else(|| "channel:email".to_string());
        khive_pack_comm::handlers::record_mailto_unsubscribe_suppression(
            runtime,
            recipient_key,
            &actor,
        )
        .await
        .map_err(|error| ChannelError::Transport(format!("{error}")))
    }

    async fn lookup_thread_owner(
        &self,
        candidate: &str,
    ) -> Result<Option<ThreadOwner>, ChannelError> {
        let runtime = self.runtime.as_ref().ok_or_else(|| {
            ChannelError::Config("external email owner lookup requires the comm runtime".into())
        })?;
        let token = runtime
            .authorize(self.namespace.clone())
            .map_err(|error| ChannelError::Transport(error.to_string()))?;
        khive_pack_comm::handlers::resolve_ingest_correlation(
            runtime,
            &token,
            &self.email_domains,
            candidate,
            true,
            khive_pack_comm::handlers::ThreadUuidProof::VerifiedOutboundEmail,
        )
        .await
        .map(|resolved| {
            resolved
                .filter(|resolved| !resolved.actor.trim().is_empty())
                .map(|resolved| ThreadOwner {
                    thread_id: resolved.thread_id.to_string(),
                    actor: resolved.actor,
                })
        })
        .map_err(|error| ChannelError::Transport(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Arc<OutboundEmailPolicy> {
        Arc::new(
            OutboundEmailPolicy::configured_for_tests(
                vec!["allowed@example.com".into()],
                vec!["actor:sender".into()],
                100,
            )
            .expect("test policy"),
        )
    }

    async fn comm_runtime() -> KhiveRuntime {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        khive_pack_comm::handlers::load_inbound_email_suppressions(&runtime)
            .await
            .expect("create suppression schema");
        runtime
    }

    /// Plant one outbound message row with the given id, returning nothing. The
    /// runtime is in-memory; the caller's namespace is `local`.
    async fn plant_outbound_message(
        runtime: &KhiveRuntime,
        id: uuid::Uuid,
        props: serde_json::Value,
    ) {
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");
        let store = runtime.notes(&token).expect("notes store");
        let now = chrono::Utc::now().timestamp_micros();
        let note = khive_storage::note::Note {
            version: 1,
            key: None,
            id,
            namespace: "local".into(),
            kind: "message".into(),
            status: "active".into(),
            name: None,
            content: "planted outbound message".into(),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(props),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        store.upsert_note(note).await.expect("upsert planted note");
    }

    /// Fixture properties for an outbound email row planted at `thread_id`. The
    /// row is an email by its `email:` addresses; it carries no `channel_kind` or
    /// `channel_slug`, which the note store refuses from this path, so its
    /// Message-ID alone decides verification.
    fn outbound_email_props(
        thread_id: &str,
        from_actor: &str,
        message_id: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "direction": "outbound",
            "from": "email:mailbox@khive.ai",
            "to": "email:user@example.com",
            "from_actor": from_actor,
            "to_actor": "email:user@example.com",
            "external_id": message_id,
            "thread_id": thread_id,
            "sent_at": chrono::Utc::now().to_rfc3339(),
        })
    }

    #[tokio::test]
    async fn thread_owner_requires_a_verified_outbound_email() {
        let runtime = comm_runtime().await;
        let host = CommExternalEmailHost::for_ingest(
            policy(),
            Some(runtime),
            Namespace::local(),
            EmailMessageIdDomains::from_mailbox_and_history("mailbox@khive.ai", "").map(Some),
        );

        // Control: a thread holding an outbound email whose Message-ID embeds the
        // row's own UUID on the configured domain yields a verified owner.
        let verified_thread = uuid::Uuid::new_v4().as_hyphenated().to_string();
        let verified_id = uuid::Uuid::new_v4();
        plant_outbound_message(
            host.runtime.as_ref().expect("runtime wired"),
            verified_id,
            outbound_email_props(
                &verified_thread,
                "lambda:owner",
                &format!("<{verified_id}@khive.ai>"),
            ),
        )
        .await;
        let owner = host
            .lookup_thread_owner(&verified_thread)
            .await
            .expect("owner lookup runs");
        assert_eq!(
            owner.map(|owner| owner.actor),
            Some("lambda:owner".to_string()),
            "a thread with a verified outbound email must yield its owner"
        );

        // The subject case: an email row that does not own its Message-ID (foreign
        // domain), so the thread-UUID path cannot prove the mailbox sent it.
        let thread_uuid = uuid::Uuid::new_v4().as_hyphenated().to_string();
        let foreign_id = uuid::Uuid::new_v4();
        plant_outbound_message(
            host.runtime.as_ref().expect("runtime wired"),
            foreign_id,
            outbound_email_props(
                &thread_uuid,
                "lambda:owner",
                &format!("<{foreign_id}@outside.example>"),
            ),
        )
        .await;
        let owner = host
            .lookup_thread_owner(&thread_uuid)
            .await
            .expect("owner lookup runs");
        assert!(
            owner.is_none(),
            "a thread UUID with no verified outbound email must yield no owner; got {owner:?}"
        );
    }

    #[tokio::test]
    async fn without_a_comm_store_nothing_is_suppressed_or_recorded() {
        let policy = policy();
        let token = policy
            .mint_unsubscribe_token("allowed@example.com")
            .expect("token for a configured recipient");
        let host = CommExternalEmailHost::new(policy, None);
        assert!(host.external_senders_configured());
        let check = host.inbound_suppression().await.expect("inbound check");
        assert!(!check("stranger@example.com"));
        let key = host
            .verify_unsubscribe_token(&token)
            .expect("token verifies");
        let error = host
            .record_mailto_unsubscribe(&key)
            .await
            .expect_err("a missing comm store must hold the opt-out for retry");
        assert!(
            matches!(&error, ChannelError::Transport(message)
                if message == "mailto unsubscribe: no comm store is wired to this channel"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_inbound_list_suppresses_no_sender() {
        let host = CommExternalEmailHost::new(policy(), Some(comm_runtime().await));
        let check = host.inbound_suppression().await.expect("inbound check");
        assert!(!check("stranger@example.com"));
    }

    #[tokio::test]
    async fn a_verified_unsubscribe_is_recorded_once_in_the_comm_store() {
        let policy = policy();
        let token = policy
            .mint_unsubscribe_token("allowed@example.com")
            .expect("token for a configured recipient");
        let host = CommExternalEmailHost::new(Arc::clone(&policy), Some(comm_runtime().await));
        let key = host
            .verify_unsubscribe_token(&token)
            .expect("token verifies");
        assert!(host
            .record_mailto_unsubscribe(&key)
            .await
            .expect("first record"));
        assert!(!host
            .record_mailto_unsubscribe(&key)
            .await
            .expect("second record"));
    }
}
