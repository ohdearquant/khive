use std::sync::Arc;

use crate::CommPack;
use khive_runtime::RuntimeError;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::types::DeleteMode;
use khive_storage::{SqlStatement, SqlValue};
use serde_json::json;
use serde_json::Value;
use uuid::Uuid;

include!("idempotency_tests/send_replay_tests.rs");

include!("idempotency_tests/concurrency_and_reply_tests.rs");

include!("idempotency_tests/key_scope_tests.rs");

include!("idempotency_tests/lifecycle_tests.rs");

include!("idempotency_tests/node_send_tests.rs");
