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
    // Dropping the awaiting phase must also stop its pending constructor,
    // even when no inherited cancellation source has fired.
    let (lifetime, cancellation) = tokio::sync::watch::channel(false);
    let result = khive_storage::scope_request_read_cancellation(cancellation, async {
        let context = khive_storage::capture_request_read_context();
        let task = tokio::task::spawn_blocking(move || {
            if context.blocking_stop_reason().is_some() {
                return Err(khive_storage::StorageError::Timeout {
                    operation: operation.into(),
                }
                .into());
            }
            // Once admitted, constructor schema work is not an interruptible read.
            Ok(context.scope_store_acquisition(operation, acquire))
        });
        join_store_task(operation, task).await
    })
    .await;
    drop(lifetime);
    result
}

pub(crate) async fn join_store_task<T>(
    operation: &'static str,
    task: tokio::task::JoinHandle<Result<T, RuntimeError>>,
) -> Result<T, RuntimeError> {
    task.await.map_err(|error| {
        khive_storage::StorageError::driver(khive_storage::StorageCapability::Sql, operation, error)
    })?
}

#[cfg(test)]
mod tests;
