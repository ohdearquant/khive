use std::io::Write;
use std::sync::Arc;

use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use tempfile::{NamedTempFile, TempDir};

use super::*;
use crate::vocab::SESSION_SCHEMA_PLAN_STMTS;

include!("ingest_tests/source_access_tests.rs");

include!("ingest_tests/jsonl_baseline_tests.rs");

include!("ingest_tests/complete_line_caps_tests.rs");

include!("ingest_tests/unterminated_progress_tests.rs");

include!("ingest_tests/cursor_commit_and_replay_tests.rs");

include!("ingest_tests/codex_ingest_tests.rs");

include!("ingest_tests/chatgpt_baseline_tests.rs");

include!("ingest_tests/chatgpt_branch_and_refusal_tests.rs");

include!("ingest_tests/chatgpt_masking_tests.rs");

include!("ingest_tests/claude_export_tests.rs");

include!("ingest_tests/export_refusal_and_rollback_tests.rs");

include!("ingest_tests/writer_queue_tests.rs");
