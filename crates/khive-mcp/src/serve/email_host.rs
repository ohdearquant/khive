//! The daemon's implementation of the email channel's external-email host.

use std::sync::Arc;

use async_trait::async_trait;
use khive_channel::ChannelError;
use khive_channel_email::{ExternalEmailHost, InboundSuppression};
use khive_runtime::{KhiveRuntime, OutboundEmailPolicy};

/// The external-email services the email channel takes from its daemon: the boot-resolved
/// outbound policy and, when one is wired, the comm store (ADR-200 sections 5.4 and 7).
pub(super) struct CommExternalEmailHost {
    policy: Arc<OutboundEmailPolicy>,
    runtime: Option<KhiveRuntime>,
}

impl CommExternalEmailHost {
    pub(super) fn new(policy: Arc<OutboundEmailPolicy>, runtime: Option<KhiveRuntime>) -> Self {
        Self { policy, runtime }
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
            return Ok(false);
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

    #[tokio::test]
    async fn without_a_comm_store_nothing_is_suppressed_or_recorded() {
        let host = CommExternalEmailHost::new(policy(), None);
        assert!(host.external_senders_configured());
        let check = host.inbound_suppression().await.expect("inbound check");
        assert!(!check("stranger@example.com"));
        let recorded = host
            .record_mailto_unsubscribe("allowed@example.com")
            .await
            .expect("record without a store");
        assert!(!recorded, "no entry is written without a comm store");
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
