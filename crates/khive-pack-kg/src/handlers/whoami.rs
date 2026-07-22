//! `whoami` verb handler.

use serde_json::{json, Value};

use khive_runtime::{actor_is_unattributed, NamespaceToken, RuntimeError};

use super::common::{deser, WhoamiParams};
use crate::KgPack;

impl KgPack {
    /// Report the identity the runtime already resolved for this request: the
    /// caller's actor reference, write namespace, and read-visible namespace
    /// set. A projection of existing `NamespaceToken` state, not new state —
    /// every field here is already computed before dispatch reaches a handler.
    ///
    /// `unattributed` uses the same id-based fallback predicate as gate
    /// checks, token minting, and comm attribution (`actor_is_unattributed`),
    /// not `ActorRef::is_anonymous()` — a configured actor whose id happens
    /// to be `"local"` is still treated as the unattributed fallback
    /// elsewhere in the runtime, and this verb must agree with that.
    pub(crate) async fn handle_whoami(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let _p: WhoamiParams = deser(params)?;
        let actor = token.actor();
        Ok(json!({
            "actor_id": actor.id,
            "actor_kind": actor.kind,
            "unattributed": actor_is_unattributed(actor),
            "namespace": token.namespace().as_str(),
            "visible_namespaces": token.visible_namespace_strs(),
        }))
    }
}
