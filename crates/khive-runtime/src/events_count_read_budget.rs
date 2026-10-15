use std::future::Future;

use khive_storage::{RequestReadDeadline, StorageError, StorageResult};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use super::EventsResponse;

#[doc(hidden)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireReadFailure {
    Timeout {
        operation: String,
    },
    AdmissionTimeout {
        operation: String,
        timeout_ms: u64,
        pool_identity: Option<String>,
    },
}

pub(super) struct CountReadBudget {
    pub(super) operator_read_budget_secs: u64,
    pub(super) deadline: RequestReadDeadline,
}

impl CountReadBudget {
    pub(super) fn capture() -> Self {
        let allowance = khive_storage::request_read_timeout_from_env();
        let deadline =
            khive_storage::effective_request_read_deadline(RequestReadDeadline::after(allowance));
        Self {
            operator_read_budget_secs: allowance.as_secs(),
            deadline,
        }
    }

    pub(super) async fn scope<T>(
        &self,
        future: impl Future<Output = StorageResult<T>>,
    ) -> StorageResult<T> {
        khive_storage::scope_request_read_deadline_at(self.deadline, async {
            khive_storage::ensure_request_read_active("events count")
                .map_err(|error| self.attribute(error))?;
            let result = future.await;
            if result.is_ok() {
                khive_storage::ensure_request_read_active("events count")
                    .map_err(|error| self.attribute(error))?;
            }
            result
        })
        .await
    }

    pub(super) fn attribute(&self, error: StorageError) -> StorageError {
        attribute_read_error(error, self.operator_read_budget_secs)
    }
}

fn attributed_operation(operation: &str, seconds: u64) -> String {
    format!("{operation} (operator read budget {seconds}s; earlier caller deadline retained)")
}

fn attribute_read_error(error: StorageError, seconds: u64) -> StorageError {
    match error {
        StorageError::Timeout { operation } => StorageError::Timeout {
            operation: attributed_operation(&operation, seconds).into(),
        },
        StorageError::AdmissionTimeout {
            operation,
            timeout_ms,
            pool_identity,
        } => StorageError::AdmissionTimeout {
            operation: attributed_operation(&operation, seconds).into(),
            timeout_ms,
            pool_identity,
        },
        other => other,
    }
}

#[cfg(unix)]
pub(super) fn count_storage_error_response(error: StorageError, seconds: u64) -> EventsResponse {
    let error = attribute_read_error(error, seconds);
    let read_failure = match &error {
        StorageError::Timeout { operation } => Some(WireReadFailure::Timeout {
            operation: operation.to_string(),
        }),
        StorageError::AdmissionTimeout {
            operation,
            timeout_ms,
            pool_identity,
        } => Some(WireReadFailure::AdmissionTimeout {
            operation: operation.to_string(),
            timeout_ms: *timeout_ms,
            pool_identity: pool_identity.clone(),
        }),
        _ => None,
    };
    let mut response = super::storage_error_response(&error);
    if let EventsResponse::Error {
        read_failure: field,
        ..
    } = &mut response
    {
        *field = read_failure;
    }
    response
}

#[cfg(unix)]
pub(super) fn restore_count_read_failure(
    response: EventsResponse,
) -> Result<EventsResponse, StorageError> {
    match response {
        EventsResponse::Error {
            message,
            retryable,
            writer_task_failure,
            sqlite_write_failure,
            read_failure: Some(failure),
        } => {
            if writer_task_failure.is_some() || sqlite_write_failure.is_some() {
                return Err(malformed_read_failure(
                    "read failure carries writer evidence",
                ));
            }
            let error = match failure {
                WireReadFailure::Timeout { operation } => StorageError::Timeout {
                    operation: operation.into(),
                },
                WireReadFailure::AdmissionTimeout {
                    operation,
                    timeout_ms,
                    pool_identity,
                } => StorageError::AdmissionTimeout {
                    operation: operation.into(),
                    timeout_ms,
                    pool_identity,
                },
            };
            if retryable != error.is_retryable() || message != error.to_string() {
                return Err(malformed_read_failure("contradictory count read failure"));
            }
            Err(error)
        }
        other => Ok(other),
    }
}

#[cfg(unix)]
pub(super) fn malformed_read_failure(message: &str) -> StorageError {
    StorageError::Serialization {
        capability: khive_storage::StorageCapability::Events,
        message: message.to_string(),
    }
}
