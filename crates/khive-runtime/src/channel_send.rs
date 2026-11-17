//! Explicit host-supplied routes for custom `comm.send` address kinds.

use std::collections::{btree_map::Entry, BTreeMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

/// The complete result of a custom send operation.
pub type ChannelSendFuture<'a> = Pin<Box<dyn Future<Output = RuntimeResult<Value>> + Send + 'a>>;

/// A trusted host component's complete custom send policy.
///
/// The comm pack validates its ordinary send parameters before calling this route.
/// `params` retains the caller's original parsed values, including optional-field
/// presence and address spelling. The runtime and namespace token are the same
/// ones authorized for the dispatch. The route owns persistence, replay handling,
/// attachments, and transport submission; no legacy message write or fallback
/// follows its result. A supplied idempotency key still passes the shared key
/// validation before the route is called.
/// Installing a route does not start a channel or a resident task.
pub trait ChannelSendRoute: Send + Sync {
    fn send<'a>(
        &'a self,
        runtime: &'a KhiveRuntime,
        token: &'a NamespaceToken,
        params: Value,
    ) -> ChannelSendFuture<'a>;
}

/// Per-runtime routes, keyed by an exact custom address kind.
///
/// Register before runtime construction. Cloning copies the registration map and
/// shares its callbacks; later registrations do not alter an existing clone.
#[derive(Clone, Default)]
pub struct ChannelSendRoutes {
    routes: BTreeMap<String, Arc<dyn ChannelSendRoute>>,
}

impl fmt::Debug for ChannelSendRoutes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChannelSendRoutes")
            .field("kinds", &self.routes.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ChannelSendRoutes {
    /// Register an exact kind without replacing a route, a built-in transport,
    /// or a local actor address kind.
    pub fn register(
        &mut self,
        kind: impl Into<String>,
        route: Arc<dyn ChannelSendRoute>,
    ) -> RuntimeResult<()> {
        let kind = kind.into();
        if kind.is_empty()
            || kind.contains(':')
            || kind.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(RuntimeError::InvalidInput(
                "channel send kind must be nonempty and contain no colon, whitespace or control characters"
                    .into(),
            ));
        }
        if [
            "khive1", "local", "email", "telegram", "actor", "lambda", "agent",
        ]
        .iter()
        .any(|reserved| kind.eq_ignore_ascii_case(reserved))
        {
            return Err(RuntimeError::InvalidInput(format!(
                "channel send kind {kind:?} is reserved"
            )));
        }
        match self.routes.entry(kind) {
            Entry::Vacant(entry) => {
                entry.insert(route);
                Ok(())
            }
            Entry::Occupied(entry) => Err(RuntimeError::InvalidInput(format!(
                "channel send kind {:?} is already registered",
                entry.key()
            ))),
        }
    }

    /// Select only an explicitly registered `kind:address` route.
    ///
    /// Matching is exact. Surrounding whitespace on a registered address is a
    /// refusal, not an invitation to normalize it or fall back to a local write.
    /// Unregistered recipients retain the comm pack's existing handling.
    pub fn resolve(&self, recipient: &str) -> RuntimeResult<Option<Arc<dyn ChannelSendRoute>>> {
        let Some((kind, _)) = recipient.trim().split_once(':') else {
            return Ok(None);
        };
        let Some(route) = self.routes.get(kind) else {
            return Ok(None);
        };
        if recipient != recipient.trim() {
            return Err(RuntimeError::InvalidInput(
                "registered channel address must not contain surrounding whitespace".into(),
            ));
        }
        Ok(Some(Arc::clone(route)))
    }
}
