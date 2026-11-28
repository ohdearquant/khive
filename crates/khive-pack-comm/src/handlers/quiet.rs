use khive_runtime::resource_admission::{request_quiet, MAX_QUIET_TTL_SECS};
use khive_runtime::RuntimeError;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuietParams {
    ttl_secs: u64,
}

pub(crate) fn handle_quiet(params: Value) -> Result<Value, RuntimeError> {
    if !params.is_object() {
        return Err(RuntimeError::InvalidInput(
            "comm.quiet parameters must be an object containing ttl_secs".into(),
        ));
    }
    let params: QuietParams = serde_json::from_value(params).map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "comm.quiet requires only ttl_secs, an integer in 1..={MAX_QUIET_TTL_SECS}"
        ))
    })?;
    if !(1..=MAX_QUIET_TTL_SECS).contains(&params.ttl_secs) {
        return Err(RuntimeError::InvalidInput(format!(
            "comm.quiet ttl_secs must be an integer in 1..={MAX_QUIET_TTL_SECS}"
        )));
    }
    let until = request_quiet(params.ttl_secs)?;
    Ok(json!({"quiet_until": until.to_rfc3339()}))
}
