use anyhow::Result;
use serde_json::Value;

pub(super) fn normalize_atomic_args(tool: &str, args: &Value) -> Result<Value> {
    let mut normalized = args.clone();
    match tool {
        "link" => khive_pack_kg::handlers::normalize_link_params(&mut normalized)?,
        "gtd.transition" => fold_task_alias(&mut normalized, "status", "to")?,
        "gtd.complete" => fold_task_alias(&mut normalized, "result", "note")?,
        _ => {}
    }
    Ok(normalized)
}

fn fold_task_alias(params: &mut Value, canonical: &str, alias: &str) -> Result<()> {
    let Some(map) = params.as_object_mut() else {
        return Ok(());
    };
    if map.contains_key(canonical) && map.contains_key(alias) {
        return Err(khive_runtime::RuntimeError::InvalidInput(format!(
            "`{alias}` is an alias for `{canonical}`; supply only one of the two, \
             even when the values agree"
        ))
        .into());
    }
    if let Some(value) = map.remove(alias) {
        map.insert(canonical.to_owned(), value);
    }
    Ok(())
}

/// ADR-099 B3 parity fix: reject unknown/typo'd arg keys on the five v1
/// atomic-admissible write verbs, BEFORE building any plan — by reusing each
/// canonical handler's own `#[serde(deny_unknown_fields)]` param struct. See
/// `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`
/// for why this exists and why it reuses rather than reimplements.
pub(super) fn validate_atomic_args(tool: &str, args: &Value) -> anyhow::Result<()> {
    fn reject<T: serde::de::DeserializeOwned>(args: &Value) -> anyhow::Result<()> {
        serde_json::from_value::<T>(args.clone())
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("bad params: {e}"))
    }

    match tool {
        // kg substrate verbs — `UpdateParams` covers both update-entity and
        // update-note (the canonical handler resolves which from `id`, not
        // from a separate struct); same struct, so one branch covers both.
        "update" => reject::<khive_pack_kg::handlers::UpdateParams>(args),
        "delete" => reject::<khive_pack_kg::handlers::DeleteParams>(args),
        "link" => reject::<khive_pack_kg::handlers::LinkParams>(args),
        // gtd verbs.
        "gtd.transition" => reject::<khive_pack_gtd::handlers::TransitionParams>(args),
        "gtd.complete" => reject::<khive_pack_gtd::handlers::CompleteParams>(args),
        _ => Ok(()),
    }
}
