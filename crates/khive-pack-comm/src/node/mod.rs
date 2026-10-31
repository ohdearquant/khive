//! Node channel host state: validated configuration, lazily initialized services and the
//! persisted outbound/pin sources, composed without I/O at construction.
//!
//! `NodeHostState::from_runtime` validates and derives everything synchronously. The key
//! facility is opened, and the pin history imported, only in `services()`, so neither the pack
//! factory nor metadata-only inspection blocks on OS I/O.

mod config;
mod pins;
mod route;
mod source;
#[cfg(test)]
mod tests;

use self::pins::RuntimePinSource;
use self::route::NodeContactLookup;
use self::source::RuntimeOutboundSource;
use async_trait::async_trait;
use khive_channel_node::adapter::NodeChannel;
use khive_channel_node::client::NodeClient;
use khive_channel_node::keys::KeyFacility;
use khive_channel_node::response::{NodeContactResult, NodeError};
use khive_channel_node::source::NodeClientBinding;
use khive_runtime::node_config::NodeChannelConfig;
use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_types::Namespace;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::OnceCell;

fn state_unavailable() -> RuntimeError {
    RuntimeError::Internal("channel.node: node state is unavailable".to_string())
}

fn key_unavailable() -> RuntimeError {
    RuntimeError::Unconfigured("channel.node: key facility is unavailable".to_string())
}

/// `NodeContactLookup` over the same `NodeClient` the channel uses.
struct ClientContact(NodeClient);

#[async_trait]
impl NodeContactLookup for ClientContact {
    async fn contact(&self, agent: uuid::Uuid) -> Result<NodeContactResult, NodeError> {
        self.0.contact(agent).await
    }
}

/// Shared host state for the node channel. Public only for host composition.
pub struct NodeHostState {
    runtime: KhiveRuntime,
    config: Arc<NodeChannelConfig>,
    binding: NodeClientBinding,
    pins: Arc<RuntimePinSource>,
    services: OnceCell<NodeServices>,
    paused: AtomicBool,
}

/// The initialized node services: one channel, one key facility, one contact lookup.
pub struct NodeServices {
    channel: Arc<NodeChannel>,
    binding: NodeClientBinding,
    lookup: Arc<dyn NodeContactLookup>,
    facility: Arc<dyn KeyFacility>,
    pins: Arc<RuntimePinSource>,
}

impl NodeHostState {
    /// Validate the enabled node configuration without I/O. `Ok(None)` when the node
    /// channel is absent or disabled.
    pub fn from_runtime(runtime: KhiveRuntime) -> Result<Option<Arc<Self>>, RuntimeError> {
        let Some(config) = runtime.config().node_channel.as_ref().filter(|c| c.enabled) else {
            return Ok(None);
        };
        let config = Arc::clone(config);
        config
            .validate()
            .map_err(|_| config::invalid("configuration is invalid"))?;
        let binding = config::binding(&config)?;
        config::poll_wait(&config)?;
        config::fingerprint_bytes(&config)?;
        config::service_url(&config)?;
        let pins = Arc::new(RuntimePinSource::new(runtime.clone(), &config, &binding)?);
        Ok(Some(Arc::new(Self {
            runtime,
            config,
            binding,
            pins,
            services: OnceCell::new(),
            paused: AtomicBool::new(false),
        })))
    }

    /// The runtime this state was built from (the comm-routed runtime).
    pub fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }

    /// The validated, enabled node configuration.
    pub fn config(&self) -> &NodeChannelConfig {
        &self.config
    }

    /// The validated sender binding; available without opening keys.
    pub fn binding(&self) -> &NodeClientBinding {
        &self.binding
    }

    /// Initialize (once) and return the node services. Order: authorize the configured
    /// namespace, retain configured pins, load the key facility off the async threads,
    /// construct the client, then the channel. A failure leaves the cell empty, so no partial
    /// channel is ever published and the next call retries.
    pub async fn services(&self) -> Result<&NodeServices, RuntimeError> {
        self.services
            .get_or_try_init(|| self.build_services())
            .await
    }

    /// Stop authenticating with the node service. The flag lives here, outside any restarted
    /// component future.
    pub fn pause_authentication(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// Whether authentication has been paused.
    pub fn authentication_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    async fn build_services(&self) -> Result<NodeServices, RuntimeError> {
        let namespace = Namespace::parse(&self.binding.namespace)
            .map_err(|_| config::invalid("namespace is invalid"))?;
        let token = self.runtime.authorize(namespace)?;
        self.pins
            .retain_configured()
            .await
            .map_err(|_| state_unavailable())?;
        let fingerprint = config::fingerprint_bytes(&self.config)?;
        let facility = load_key_facility(self.binding.key_reference.clone(), fingerprint).await?;
        let source = Arc::new(RuntimeOutboundSource::new(
            self.runtime.clone(),
            token,
            self.binding.clone(),
        ));
        let client = NodeClient::new(
            self.binding.clone(),
            config::service_url(&self.config)?,
            source,
            self.pins.clone(),
            facility.clone(),
        )
        .map_err(|_| config::invalid("client construction failed"))?;
        let lookup: Arc<dyn NodeContactLookup> = Arc::new(ClientContact(client.clone()));
        let channel = Arc::new(NodeChannel::new(client, config::poll_wait(&self.config)?));
        Ok(NodeServices {
            channel,
            binding: self.binding.clone(),
            lookup,
            facility,
            pins: self.pins.clone(),
        })
    }

    /// Test seam: publish fake services (a channel built over a test client, a fake contact
    /// lookup and key facility). Returns false when services were already initialized.
    #[cfg(test)]
    pub(crate) fn install_test_services(
        &self,
        channel: Arc<NodeChannel>,
        lookup: Arc<dyn NodeContactLookup>,
        facility: Arc<dyn KeyFacility>,
    ) -> bool {
        self.services
            .set(NodeServices {
                channel,
                binding: self.binding.clone(),
                lookup,
                facility,
                pins: self.pins.clone(),
            })
            .is_ok()
    }
}

impl NodeServices {
    /// The shared channel adapter.
    pub fn channel(&self) -> Arc<NodeChannel> {
        self.channel.clone()
    }

    /// The binding these services were built for.
    pub fn binding(&self) -> &NodeClientBinding {
        &self.binding
    }

    /// Contact lookup over the same client the channel uses.
    pub(crate) fn lookup(&self) -> &Arc<dyn NodeContactLookup> {
        &self.lookup
    }

    /// The key facility (private-key operations only; never key bytes).
    pub(crate) fn key_facility(&self) -> &Arc<dyn KeyFacility> {
        &self.facility
    }

    /// The pin source the client verifies against, for independent exact-key resolution.
    pub(crate) fn pin_source(&self) -> &Arc<RuntimePinSource> {
        &self.pins
    }
}

/// Open the configured keychain entry off the async threads and require its public
/// fingerprint to equal the configured one.
#[cfg(target_os = "macos")]
async fn load_key_facility(
    reference: String,
    fingerprint: [u8; 32],
) -> Result<Arc<dyn KeyFacility>, RuntimeError> {
    let facility = tokio::task::spawn_blocking(move || {
        khive_channel_node::keychain::MacOsKeyFacility::open(&reference, &fingerprint)
    })
    .await
    .map_err(|_| key_unavailable())?
    .map_err(|_| key_unavailable())?;
    let facility: Arc<dyn KeyFacility> = Arc::new(facility);
    if facility.public_keys().fingerprint().as_bytes() != &fingerprint {
        return Err(key_unavailable());
    }
    Ok(facility)
}

/// No OS key facility exists on this target.
#[cfg(not(target_os = "macos"))]
async fn load_key_facility(
    reference: String,
    fingerprint: [u8; 32],
) -> Result<Arc<dyn KeyFacility>, RuntimeError> {
    let _ = (reference, fingerprint);
    Err(key_unavailable())
}
