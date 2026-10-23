//! Sealed grants, authenticated durable envelopes and capability validation.
//!
//! This library preserves actor bookkeeping and explicit operation classification
//! over one keyed capability kernel. It does not install a runtime gate or grant
//! store. See the README for the trusted management and seal-verification boundary.

pub mod actor;
pub mod capability;
pub mod context;
pub mod durable;
pub mod error;
pub mod namespace;
pub mod validate;

pub use actor::ActorRegistry;
pub use capability::CapabilityManager;
pub use context::{RequestContext, ScopeLevel};
pub use error::KernelError;
pub use namespace::NamespaceMap;
pub use validate::RequestValidator;

pub use lion_core::{CapId, Capability, PluginId, Right, Rights, SecurityLevel};
