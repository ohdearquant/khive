//! External-email services the email channel takes from its host.

use std::sync::Arc;

use async_trait::async_trait;
use khive_channel::ChannelError;

use crate::external_inbound::ThreadOwner;

/// External-email services the channel takes from its host (ADR-200 sections 5.4 and 7).
///
/// The channel depends on no store or policy crate; the daemon implements this over its comm
/// store and the boot-resolved outbound policy.
#[async_trait]
pub trait ExternalEmailHost: Send + Sync {
    /// Whether external senders are configured.
    fn external_senders_configured(&self) -> bool;

    /// Whether authenticated non-maintainer mail is admitted as external inbound (ADR-200 section
    /// 7 step 5). While it is not, the channel reads no inbound suppression list and every
    /// non-maintainer sender stays off the allowlist. Configured external senders always enable
    /// it; a host can also enable it alone, so external mail is received while outbound external
    /// sending stays disabled.
    fn external_inbound_enabled(&self) -> bool {
        self.external_senders_configured()
    }

    /// The recipient key a mailto unsubscribe token verifies to, or `None` when it does not verify.
    fn verify_unsubscribe_token(&self, token: &str) -> Option<String>;

    /// Load the inbound suppression check for one poll: the returned predicate takes a
    /// normalized sender address and answers whether an inbound entry suppresses it.
    async fn inbound_suppression(&self) -> Result<InboundSuppression, ChannelError>;

    /// Resolve one sender-asserted candidate only through verified outbound ownership.
    async fn lookup_thread_owner(
        &self,
        candidate: &str,
    ) -> Result<Option<ThreadOwner>, ChannelError>;

    /// Record a verified mailto unsubscribe for `recipient_key`. `Ok(true)` when a new entry was
    /// written, `Ok(false)` when an identical entry already existed.
    async fn record_mailto_unsubscribe(&self, recipient_key: &str) -> Result<bool, ChannelError>;
}

/// A loaded inbound suppression check (see [`ExternalEmailHost::inbound_suppression`]).
pub type InboundSuppression = Arc<dyn Fn(&str) -> bool + Send + Sync>;
