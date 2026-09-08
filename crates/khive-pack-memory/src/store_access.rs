use khive_runtime::RuntimeError;

pub(crate) async fn acquire_store<T, F>(
    operation: &'static str,
    acquire: F,
) -> Result<T, RuntimeError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    // Store constructors may wait for SQLite's writer. An async snapshot
    // owner must be able to run and release that connection while they wait.
    khive_storage::ensure_request_read_active(operation)?;
    let context = khive_storage::capture_request_read_context();
    tokio::task::spawn_blocking(move || {
        if context.stop_reason().is_some() {
            return Err(khive_storage::StorageError::Timeout {
                operation: operation.into(),
            }
            .into());
        }
        // Once admitted, constructor schema work is not an interruptible read.
        Ok(acquire())
    })
    .await
    .map_err(|error| {
        RuntimeError::Internal(format!(
            "{operation} store acquisition task failed: {error}"
        ))
    })?
}

#[cfg(test)]
mod tests;
