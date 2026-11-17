//! Explicit channel composition for one host boot.

use std::sync::Arc;

use khive_channel::{Channel, ChannelRegistry};

/// Caller-owned channels to compose with the host's configured builtin channels.
///
/// Entries use the channel's exact `(kind, slug)` identity. Registration does not
/// start polling or send messages: a caller-owned driver remains responsible for
/// its channel. Builtin loops continue to drive only their own selected entries.
/// No optional transport feature is needed to construct this bundle.
#[derive(Clone, Default)]
pub struct ServeRegistrations {
    channels: Vec<Arc<dyn Channel>>,
}

impl ServeRegistrations {
    /// Create an empty per-boot registration bundle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one channel, refusing a duplicate caller identity rather than replacing it.
    ///
    /// A collision with a configured builtin is checked during host preparation,
    /// before any background task starts. Channel identities must remain stable.
    pub fn register_channel(&mut self, channel: Arc<dyn Channel>) -> anyhow::Result<()> {
        if self
            .channels
            .iter()
            .any(|existing| existing.kind() == channel.kind() && existing.slug() == channel.slug())
        {
            anyhow::bail!(
                "duplicate channel registration ({}, {})",
                channel.kind(),
                channel.slug()
            );
        }
        self.channels.push(channel);
        Ok(())
    }

    pub(super) fn merge_channels(
        &self,
        builtins: impl IntoIterator<Item = Arc<dyn Channel>>,
    ) -> anyhow::Result<Arc<ChannelRegistry>> {
        let mut registry = ChannelRegistry::new();
        for channel in builtins.into_iter().chain(self.channels.iter().cloned()) {
            if registry
                .get_by_slug(channel.kind(), &channel.slug())
                .is_some()
            {
                anyhow::bail!(
                    "duplicate channel registration ({}, {})",
                    channel.kind(),
                    channel.slug()
                );
            }
            registry.register(channel);
        }
        Ok(Arc::new(registry))
    }
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
pub(super) fn selected_channel(
    channels: &ChannelRegistry,
    expected: &Arc<dyn Channel>,
) -> Result<Arc<dyn Channel>, crate::components::ComponentError> {
    let selected = channels
        .get_by_slug(expected.kind(), &expected.slug())
        .filter(|selected| Arc::ptr_eq(selected, expected));
    selected.ok_or_else(|| {
        crate::components::ComponentError::Permanent(format!(
            "registered channel owner mismatch ({}, {})",
            expected.kind(),
            expected.slug()
        ))
    })
}

#[cfg(all(test, any(feature = "channel-email", feature = "channel-telegram")))]
pub(super) fn isolated_test_registry(channel: Arc<dyn Channel>) -> Arc<ChannelRegistry> {
    ServeRegistrations::default()
        .merge_channels([channel])
        .expect("isolated channel fixture registry")
}
