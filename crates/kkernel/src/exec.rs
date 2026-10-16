//! `kkernel exec` — run a verb DSL expression directly through the pack registry.
//!
//! When the warm daemon is reachable, exec forwards through it instead of
//! building an in-process runtime (ADR-049). Config and namespace are matched
//! against the daemon's own fingerprint; a mismatch falls back to local
//! dispatch, keeping behaviour identical to the in-process path.
//! Accepted daemon results disclose that logging is configured separately:
//! `--log` and `KHIVE_LOG` affect the client process, while the daemon's level
//! is fixed at startup. The response protocol does not report a daemon PID
//! or stderr destination, so this disclosure includes neither.
//! A forwarded lexical-timeout response is also reported as a client-side
//! WARN: the daemon's detailed event cannot reach the caller's stderr.
//!
//! ## Modes
//!
//! - **DSL mode** (default): `kkernel exec '<dsl>'` — executes a single verb DSL
//!   expression or batch against the configured database and namespace.
//! - **Plan mode**: `kkernel exec --plan '<dsl>'` — parses through an already
//!   running daemon and prints its plan without dispatch or local construction.
//! - **Pending-events mode**: `kkernel exec --pending-events` — one-shot drain that
//!   fires all due `scheduled_event` notes. Mutually exclusive with the positional
//!   `ops` argument. Cron-friendly: run every minute for minute-granularity delivery.
//!
//! # `--ops-file` bulk-apply path
//!
//! `kkernel exec --ops-file batch.jsonl` reads a JSONL file where each
//! non-blank line is a JSON op object `{"tool":"verb","args":{...}}`.  All
//! lines are validated first into a bounded temporary snapshot; a malformed
//! line aborts before any writes without retaining the whole file in memory.
//! Physical lines are capped at 96 MiB and the file at 512 MiB. Valid ops are
//! dispatched in chunks of at most 100 and 32 MiB (one larger op runs alone)
//! through the same
//! in-process runtime path (daemon fast-path is intentionally skipped for
//! bulk apply — the daemon is warm-state optimised, not throughput optimised).
//! A progress line is printed per chunk. `--save-file` streams ordered rows to
//! a sink whose final-file publication is atomic; the database chunks commit
//! incrementally. After dispatch begins, success and failure both print a
//! reconciliation manifest. An aborted manifest names confirmed committed
//! chunks and any dispatched chunk whose response could not be verified.
//! Without `--save-file`, validated row payloads are discarded after aggregation.
//! `--serial` keeps the same logical chunks and one warm server/model instance,
//! but runs each full chunk through the same batch path with handler concurrency
//! capped at one. The default remains bounded parallel execution; `--serial`
//! conflicts with `--atomic`.
//! `--dry-run` validates every line and prints a per-verb summary without writes.

#[cfg(test)]
use std::io::Write as _;
#[cfg(all(test, unix))]
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
use anyhow::Result;
#[cfg(test)]
use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
#[cfg(all(test, unix))]
use khive_mcp::server::compute_config_id;
#[cfg(test)]
use khive_mcp::server::KhiveMcpServer;
#[cfg(test)]
use khive_mcp::tools::request::RequestParams;
#[cfg(all(test, unix))]
use khive_runtime::DaemonRequestFrame;
#[cfg(test)]
use khive_runtime::{KhiveConfig, KhiveRuntime, Namespace, RuntimeConfig};
#[cfg(test)]
use khive_types::RefusalReason;

mod args;
mod ops_file_apply;
mod ops_file_validate;
mod plan;
mod refusal;
mod run;

use ops_file_apply::should_defer_chunk_entry;
#[cfg(test)]
use ops_file_apply::{
    apply_ops_file, apply_ops_file_reader_with_dispatch_mode, apply_ops_file_with_dispatch_mode,
    apply_ops_file_with_response_transform, collect_op_failures, ops_file_progress_line,
    ops_file_summary, retain_failure_detail, validate_ordered_chunk_envelope, AbortedOpsFileError,
    OpsFileDispatchMode, OpsFileReportMode,
};

#[cfg(test)]
pub(crate) use ops_file_validate::parse_ops_file;
pub(crate) use ops_file_validate::OpsFileEntry;
#[cfg(test)]
use ops_file_validate::{
    parse_atomic_validated_snapshot, read_bounded_ops_line_with_limit, validate_ops_file,
};
use ops_file_validate::{parse_ops_file_line, read_bounded_ops_line};

pub(crate) use refusal::acquire_local_construction_guard;
use refusal::refusal_error;
#[cfg(test)]
use refusal::ExecRefusal;
#[cfg(all(test, unix))]
use refusal::ForwardFuture;

pub use args::ExecArgs;
use run::annotate_and_emit_refusals;
pub use run::run_exec;
#[cfg(test)]
use run::{
    apply_actor_pin_and_expectation, build_local_fallback_server, enforce_strict_batch_result,
    prepare_exec_output, render_atomic_output, run_exec_inline, run_exec_inline_with_forward,
    run_exec_ops_file,
};
#[cfg(any(unix, test))]
use run::{load_exec_config, ExecDbContext};

// `khive-request` is not a direct kkernel dependency.  We use serde_json to
// parse JSONL lines directly (the format is a strict subset of JSON form)
// rather than pulling in the full DSL parser crate.

/// Chunk size for `--ops-file` bulk dispatch.
///
/// Each chunk is dispatched as a single parallel batch through the same
/// `dispatch_request_local` path the MCP `request` tool uses.  100 matches
/// [`khive_request::MAX_OPS`] so the batch always fits inside the parser limit.
const OPS_FILE_CHUNK_SIZE: usize = 100;

/// Large payloads reduce the op count per dispatch so a 100-op chunk cannot
/// duplicate/stringify the entire accepted input snapshot at once. One op may
/// exceed this budget (up to the physical-line ceiling) and runs alone.
const OPS_FILE_CHUNK_MAX_BYTES: usize = 32 * 1024 * 1024;

/// A single JSONL op may carry one 64 MiB Moodboard object as base64 plus its
/// bounded metadata, but cannot grow without limit before JSON validation.
const MAX_OPS_FILE_LINE_BYTES: usize = 96 * 1024 * 1024;

/// The validated on-disk snapshot bounds both disk amplification and the
/// all-in-memory atomic path. Non-atomic execution retains only one chunk.
const MAX_OPS_FILE_BYTES: u64 = 512 * 1024 * 1024;

const MAX_OPS_FILE_FAILURE_DETAILS: usize = 1_000;
const MAX_OPS_FILE_FAILURE_ERROR_BYTES: usize = 4 * 1024;

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
