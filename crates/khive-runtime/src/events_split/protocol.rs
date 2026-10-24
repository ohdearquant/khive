use super::events_count_read_budget::WireReadFailure;
use khive_storage::event::{
    EventGroupBy, EventPageQuery, EventPageWindow, IdempotentEventBatchResult,
};
use khive_storage::{BatchWriteSummary, Event, EventFilter, Page, PageRequest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Request frame sent from a domain process to the events daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EventsRequest {
    /// Fire-and-forget append lane. The daemon replies with a summary, but the
    /// forwarder treats a failed reply as a counted drop, never an error to
    /// the original caller.
    AppendEvents {
        protocol_version: u32,
        namespace: String,
        events: Vec<Event>,
    },
    /// ADR-133 audit-batch lane: real dispositions come back.
    AppendEventsIdempotent {
        protocol_version: u32,
        namespace: String,
        events: Vec<Event>,
    },
    GetEvent {
        protocol_version: u32,
        namespace: String,
        id: Uuid,
    },
    QueryEvents {
        protocol_version: u32,
        namespace: String,
        filter: EventFilter,
        page: PageRequest,
    },
    QueryEventPage {
        protocol_version: u32,
        namespace: String,
        query: EventPageQuery,
    },
    CountEvents {
        protocol_version: u32,
        namespace: String,
        #[serde(default)]
        operator_read_budget_secs: Option<u64>,
        filter: EventFilter,
    },
    CountEventsGrouped {
        protocol_version: u32,
        namespace: String,
        #[serde(default)]
        operator_read_budget_secs: Option<u64>,
        filter: EventFilter,
        group_by: EventGroupBy,
    },
}

impl EventsRequest {
    pub(super) fn protocol_version(&self) -> u32 {
        match self {
            Self::AppendEvents {
                protocol_version, ..
            }
            | Self::AppendEventsIdempotent {
                protocol_version, ..
            }
            | Self::GetEvent {
                protocol_version, ..
            }
            | Self::QueryEvents {
                protocol_version, ..
            }
            | Self::QueryEventPage {
                protocol_version, ..
            }
            | Self::CountEvents {
                protocol_version, ..
            }
            | Self::CountEventsGrouped {
                protocol_version, ..
            } => *protocol_version,
        }
    }

    pub(super) fn count_budget_secs(&self) -> Option<Option<u64>> {
        match self {
            Self::CountEvents {
                operator_read_budget_secs,
                ..
            }
            | Self::CountEventsGrouped {
                operator_read_budget_secs,
                ..
            } => Some(*operator_read_budget_secs),
            _ => None,
        }
    }

    pub(super) fn namespace(&self) -> &str {
        match self {
            Self::AppendEvents { namespace, .. }
            | Self::AppendEventsIdempotent { namespace, .. }
            | Self::GetEvent { namespace, .. }
            | Self::QueryEvents { namespace, .. }
            | Self::QueryEventPage { namespace, .. }
            | Self::CountEvents { namespace, .. }
            | Self::CountEventsGrouped { namespace, .. } => namespace,
        }
    }
}

/// Response frame from the events daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventsResponse {
    Appended {
        summary: BatchWriteSummary,
    },
    Idempotent {
        result: IdempotentEventBatchResult,
    },
    Event {
        event: Option<Event>,
    },
    Pageful {
        page: Page<Event>,
    },
    EventPageWindow {
        window: EventPageWindow,
    },
    Count {
        count: u64,
    },
    GroupedCount {
        counts: BTreeMap<String, u64>,
    },
    /// Typed refusal. `retryable` distinguishes transient daemon-side
    /// conditions from contract errors (bad frame, version skew).
    /// `writer_task_failure` carries both request finality and whether the
    /// remote writer seam terminated. The retryable bit cannot express that
    /// distinction: an ordinary operation or COMMIT error may be transient or
    /// permanent while its transaction is independently proven rolled back.
    /// `None` means the error is not from a writer request. Cross-version
    /// frames never reach this mapping because `dispatch_events_request`
    /// refuses a mismatched version before any write executes.
    Error {
        message: String,
        retryable: bool,
        #[serde(default)]
        writer_task_failure: Option<WireWriterTaskFailure>,
        /// Optional native write evidence; older frames retain their legacy mapping.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sqlite_write_failure: Option<khive_storage::error::SqliteWriteFailure>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        read_failure: Option<WireReadFailure>,
    },
}

/// Whether a writer-state error belongs to one failed request on a still-live
/// seam or to a permanently retired writer task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireWriterTaskFailure {
    RequestFailed {
        request_state: WireWriterTaskState,
    },
    TaskTerminated {
        request_state: WireWriterTaskState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sqlite_full_codes: Option<(i32, i32)>,
    },
}

/// Wire mirror of [`khive_storage::WriterTaskRequestState`]. A separate type
/// because the storage enum is not serializable and the wire shape must stay
/// under this module's protocol-version control, not the storage crate's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireWriterTaskState {
    NotStarted,
    TransactionRolledBack,
    SideEffectsUnknown,
}

impl From<khive_storage::WriterTaskRequestState> for WireWriterTaskState {
    fn from(state: khive_storage::WriterTaskRequestState) -> Self {
        use khive_storage::WriterTaskRequestState as S;
        match state {
            S::NotStarted => Self::NotStarted,
            S::TransactionRolledBack => Self::TransactionRolledBack,
            S::SideEffectsUnknown => Self::SideEffectsUnknown,
        }
    }
}

impl From<WireWriterTaskState> for khive_storage::WriterTaskRequestState {
    fn from(state: WireWriterTaskState) -> Self {
        use khive_storage::WriterTaskRequestState as S;
        match state {
            WireWriterTaskState::NotStarted => S::NotStarted,
            WireWriterTaskState::TransactionRolledBack => S::TransactionRolledBack,
            WireWriterTaskState::SideEffectsUnknown => S::SideEffectsUnknown,
        }
    }
}
