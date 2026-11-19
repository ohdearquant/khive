use std::future::Future;
use std::pin::Pin;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use serde::Deserialize;
use serde_json::Value;

use crate::canonical_apply::{apply_delta, decode_delta, preimage_status, CanonicalApplyOutcome};
use crate::KgPack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyChangesetParams {
    delta: String,
}

impl KgPack {
    /// [`Self::handle_apply_changeset`] behind one heap allocation. A plain function builds the
    /// future and moves it into the box, so the large unpolled future never occupies the stack
    /// frame of the async dispatch that awaits it.
    pub(crate) fn handle_apply_changeset_boxed<'a>(
        &'a self,
        token: &'a NamespaceToken,
        params: Value,
        registry: &'a VerbRegistry,
    ) -> Pin<Box<dyn Future<Output = Result<Value, RuntimeError>> + Send + 'a>> {
        Box::pin(self.handle_apply_changeset(token, params, registry))
    }

    pub(crate) async fn handle_apply_changeset(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let params: ApplyChangesetParams = super::common::deser(params)?;
        let changeset = decode_delta(&params.delta, None)?;
        let preimage = preimage_status(&changeset);
        let operations: Vec<Value> = changeset.ops.iter().enumerate().map(|(index, op)| {
            let (verb, id, checked) = match op {
                khive_changeset::Op::Create(op) => ("create", op.id, false),
                khive_changeset::Op::Link(op) => ("link", op.id, false),
                khive_changeset::Op::Update(op) => ("update", op.target_id(), true),
                khive_changeset::Op::Delete(op) => ("delete", op.target_id, true),
                khive_changeset::Op::Restore(op) => ("restore", op.target_id(), false),
                khive_changeset::Op::Merge(_) => unreachable!("decode rejects merge"),
            };
            serde_json::json!({"op_index":index,"verb":verb,"id":id,"preimage":if checked {"captured"} else {"none"}})
        }).collect();
        match apply_delta(&self.runtime, registry, token, &params.delta, None).await? {
            CanonicalApplyOutcome::Committed {
                created_records,
                reconciliation,
                ..
            } => Ok(serde_json::json!({
                "committed":true,"operations":operations,"created_records":created_records,
                "preimage":preimage,"reconciliation_required":reconciliation.is_some(),
                "retryable":false,
            })),
            CanonicalApplyOutcome::RolledBack { error }
            | CanonicalApplyOutcome::OutcomeUnknown { error } => Err(error),
        }
    }
}
