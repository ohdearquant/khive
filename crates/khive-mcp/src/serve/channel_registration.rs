//! Explicit channel composition for one host boot.

use std::sync::Arc;

use khive_channel::{Channel, ChannelRegistry};
use khive_runtime::channel_section::{ChannelConfigValidator, ChannelConfigValidators};
use khive_runtime::channel_send::{ChannelSendRoute, ChannelSendRoutes};
use khive_runtime::RuntimeConfig;

/// Caller-owned channels to compose with the host's configured builtin channels.
///
/// Entries use the channel's exact `(kind, slug)` identity. Registration does not
/// start polling or send messages: a caller-owned driver remains responsible for
/// its channel. Builtin loops continue to drive only their own selected entries.
/// No optional transport feature is needed to construct this bundle.
#[derive(Clone, Default)]
pub struct ServeRegistrations {
    channels: Vec<Arc<dyn Channel>>,
    residents: Vec<crate::components::ResidentComponentRegistration>,
    config_validators: Vec<(String, ChannelConfigValidator)>,
    send_routes: Vec<(String, Arc<dyn ChannelSendRoute>)>,
}

impl ServeRegistrations {
    /// Create an empty per-boot registration bundle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a component's pure, idempotent configuration check.
    ///
    /// The callback receives its exact opaque value, or `None`, before runtime
    /// construction, even when comm is not loaded. Pack activation may validate
    /// again; callbacks must not start work or depend on exactly one invocation.
    pub fn register_config_validator(
        &mut self,
        name: impl Into<String>,
        validator: ChannelConfigValidator,
    ) -> anyhow::Result<()> {
        let name = name.into();
        let mut proposed = ChannelConfigValidators::default();
        for (existing_name, existing) in &self.config_validators {
            proposed.register(existing_name.clone(), existing.clone())?;
        }
        proposed.register(name.clone(), validator.clone())?;
        self.config_validators.push((name, validator));
        Ok(())
    }

    /// Install a trusted component's complete send policy for a custom address kind.
    ///
    /// The callback owns persistence, replay handling, and transport submission.
    /// Built-in transport and local actor address kinds cannot be overridden.
    /// Use [`super::run_with_registrations`] so routes reach the runtime before
    /// pack construction. Registration itself does not call or start the route.
    pub fn register_send_route(
        &mut self,
        kind: impl Into<String>,
        route: Arc<dyn ChannelSendRoute>,
    ) -> anyhow::Result<()> {
        let kind = kind.into();
        let mut proposed = ChannelSendRoutes::default();
        for (existing_kind, existing) in &self.send_routes {
            proposed.register(existing_kind.clone(), existing.clone())?;
        }
        proposed.register(kind.clone(), route.clone())?;
        self.send_routes.push((kind, route));
        Ok(())
    }

    pub(super) fn install_runtime_config(&self, config: &mut RuntimeConfig) -> anyhow::Result<()> {
        let mut proposed = config.channel_config_validators.clone();
        for (name, validator) in &self.config_validators {
            proposed.register(name.clone(), validator.clone())?;
        }
        let mut routes = config.channel_send_routes.clone();
        for (kind, route) in &self.send_routes {
            routes.register(kind.clone(), route.clone())?;
        }
        proposed.validate(&config.channel)?;
        config.channel_config_validators = proposed;
        config.channel_send_routes = routes;
        Ok(())
    }

    pub(super) fn require_prebuilt_compatible(&self) -> anyhow::Result<()> {
        if !self.config_validators.is_empty() || !self.send_routes.is_empty() {
            anyhow::bail!(
                "configuration validators and send routes must be installed before server construction; \
                 use run_with_registrations, or configure RuntimeConfig before building \
                 and pass only channels/residents to serve_server_with_registrations"
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn require_local_dispatch(
        &self,
        args: &crate::args::Args,
        daemon_disabled: bool,
    ) -> anyhow::Result<()> {
        // Request forwarding does not depend on the transport: a non-daemon
        // host with a file-backed store forwards to the warm daemon, which has
        // none of this process's validators or routes.
        if (!self.config_validators.is_empty() || !self.send_routes.is_empty())
            && !args.daemon
            && !daemon_disabled
        {
            anyhow::bail!(
                "caller configuration validators and send routes cannot be forwarded to a daemon; \
                 use a daemon host or set KHIVE_NO_DAEMON=1 for local dispatch"
            );
        }
        Ok(())
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

    /// Register a captured resident for persistent-daemon startup.
    ///
    /// Names cannot collide with builtin, linked, or other caller components.
    /// The trusted resident owns its runtime policy; registration does not infer
    /// write authority from the host default runtime. Stdio and demand boots skip it.
    pub fn register_resident(
        &mut self,
        resident: crate::components::ResidentComponentRegistration,
    ) -> anyhow::Result<()> {
        let mut proposed = self.residents.clone();
        proposed.push(resident);
        crate::components::validate_resident_components(&proposed)?;
        self.residents = proposed;
        Ok(())
    }

    pub(super) fn resident_roster(
        &self,
    ) -> anyhow::Result<Vec<crate::components::ResidentComponentRegistration>> {
        crate::components::validate_resident_components(&self.residents)?;
        Ok(self.residents.clone())
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
