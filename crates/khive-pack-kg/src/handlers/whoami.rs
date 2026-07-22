//! `whoami` verb handler.

use serde_json::{json, Value};

use khive_runtime::{NamespaceToken, RuntimeError};

use super::common::{deser, WhoamiParams};
use crate::KgPack;

impl KgPack {
    /// Report the identity the runtime already resolved for this request: the
    /// caller's actor reference, write namespace, and read-visible namespace
    /// set. A projection of existing `NamespaceToken` state, not new state —
    /// every field here is already computed before dispatch reaches a handler.
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
            "unattributed": actor.is_anonymous(),
            "namespace": token.namespace().as_str(),
            "visible_namespaces": token.visible_namespace_strs(),
        }))
    }
}
