use crate::{RuntimeError, RuntimeResult};

pub(crate) fn validate_key(key: &str, refusal: &str) -> RuntimeResult<()> {
    if key.len() > 512 || key.contains('\0') {
        return Err(RuntimeError::InvalidInput(refusal.into()));
    }
    Ok(())
}

pub(crate) fn validate_fence_count(
    count: usize,
    limit: usize,
    refusal: impl FnOnce() -> String,
) -> RuntimeResult<()> {
    if count > limit {
        return Err(RuntimeError::InvalidInput(refusal()));
    }
    Ok(())
}
