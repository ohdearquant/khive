use std::path::PathBuf;

use clap::Parser;

/// Arguments for `kkernel exec` — execute a verb DSL expression against a chosen
/// database and namespace, the same syntax accepted by the MCP `request` tool.
#[derive(Parser, Debug)]
pub struct ExecArgs {
    /// DSL expression to execute (same syntax as MCP `request` tool).
    ///
    /// Examples:
    ///   kkernel exec 'knowledge.stats()'
    ///   kkernel exec 'knowledge.index(rebuild_ann=true)'
    ///   kkernel exec '[knowledge.list(limit=5), knowledge.stats()]'
    ///
    /// Mutually exclusive with `--pending-events` and `--ops-file`.
    pub ops: Option<String>,

    /// Check grammar and list stages without executing operations.
    ///
    /// Requires an already-running daemon with matching configuration. Prints
    /// the plan as JSON; a grammar error is a successful `parsed=false` result.
    #[arg(
        long,
        requires = "ops",
        conflicts_with_all = [
            "presentation", "strict", "output_format", "save_file", "ops_file",
            "pending_events", "dry_run", "serial", "atomic", "atomic_max_ops",
            "verbose", "actor", "expect_actor", "namespace"
        ]
    )]
    pub plan: bool,

    /// One-shot drain: fire all `scheduled_event` notes whose `trigger_at <= now`.
    ///
    /// Scans all namespaces, dispatches each event's action in its own namespace,
    /// marks fired events, and advances repeating events (daily/weekly/monthly).
    /// Prints a JSON summary of scanned/fired/advanced/failed counts to stdout.
    ///
    /// Mutually exclusive with the positional `ops` argument and `--ops-file`.
    /// Suitable for cron:
    ///   * * * * *  kkernel exec --pending-events
    #[arg(long, conflicts_with = "ops", conflicts_with = "ops_file")]
    pub pending_events: bool,

    /// Database path (defaults to `~/.khive/khive.db`). `:memory:` selects an
    /// ephemeral in-memory database, matching `kkernel mcp`.
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,

    /// Explicit khive configuration file. This selects the same engine,
    /// backend-topology, and actor configuration used by `kkernel mcp`.
    #[arg(long, env = "KHIVE_CONFIG")]
    pub config: Option<PathBuf>,

    /// Namespace to operate in.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Pin the acting identity for this invocation.
    ///
    /// This is an attribution and authorization identity, not a storage
    /// namespace. It has higher precedence than project config and
    /// `KHIVE_ACTOR`, and is checked by the same dispatch gate as every other
    /// resolved actor. A refused actor is never retried as a fallback identity.
    #[arg(long, value_name = "ACTOR", conflicts_with = "pending_events")]
    pub actor: Option<String>,

    /// Require the resolved acting identity to equal this value.
    ///
    /// Composes with `--actor`; without it, validates the normal project,
    /// config, and environment resolution chain. Use `local` to require the
    /// anonymous/local identity. A mismatch fails before dispatch.
    #[arg(long, value_name = "ACTOR", conflicts_with = "pending_events")]
    pub expect_actor: Option<String>,

    /// Presentation mode: `verbose` (default), `agent`, or `human`.
    ///
    /// ADR-045 §2 selection rules: the `kkernel exec` CLI surface (a trusted
    /// operator / scripted-caller path) defaults to `Verbose` — the full
    /// canonical shape — unlike the MCP `request` tool, which defaults to
    /// `Agent` for token efficiency. Pass `--presentation agent` to opt into
    /// the trimmed shape, or `--presentation human` for pretty terminal output.
    #[arg(
        long,
        default_value = "verbose",
        default_value_if("plan", "true", None)
    )]
    pub presentation: Option<String>,

    /// Output format for verb results (ADR-078 §2 precedence: this flag >
    /// `KHIVE_OUTPUT_FORMAT` env var > `[runtime] default_output_format` in
    /// `khive.toml` > builtin `json`).
    ///
    /// Valid values: `json` (compact, lossless — default), `auto` (shape-aware:
    /// markdown table for record arrays, key-value block for single records),
    /// `table` (force markdown table).
    ///
    /// The legacy `--ops-file` path without `--save-file` keeps its established
    /// aggregate JSON summary and does not forward this override to transient
    /// rows. Combined bulk save always persists lossless JSON rows, matching
    /// inline save.
    #[arg(long, value_name = "FORMAT")]
    pub output_format: Option<String>,

    /// Verbose output: print per-event progress to stderr.
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Write results as JSONL to this path and print a self-describing manifest.
    ///
    /// The manifest (`{path, rows, per_column_null_counts, schema_fingerprint,
    /// checksum, summary, failures?}`) is printed to stdout instead of the raw
    /// results. Optional `failures` entries project each failed row's error and
    /// any stable reason. With `--ops-file`, ordered per-op envelopes from every
    /// chunk are retained in one JSONL file. Database chunks commit incrementally;
    /// after dispatch begins every exit prints a reconciliation manifest. A
    /// post-dispatch failure prints `status="aborted"`, the confirmed committed
    /// chunks, and any dispatched-but-unverified chunk. Its incomplete temp file
    /// is discarded, so a prior destination remains unchanged. Parent directories
    /// are created if absent.
    ///
    /// Note: `--save-file` always runs in-process and bypasses the warm daemon,
    /// so ANN-dependent verbs (e.g. `knowledge.suggest`, `knowledge.compose`) may
    /// hit a cold or warming index on the first call after a daemon restart.
    ///
    /// Example:
    ///   kkernel exec 'list(kind="entity")' --save-file /tmp/entities.jsonl
    #[arg(long, conflicts_with = "dry_run")]
    pub save_file: Option<String>,

    /// JSONL file of ops to apply in bulk.
    ///
    /// Each non-blank line must be a JSON object `{"tool":"verb","args":{...}}`
    /// (the same JSON form the MCP `request` tool accepts).  All lines are
    /// parsed before any write.  A malformed line prints the line number and
    /// error, then aborts without writing.
    ///
    /// The source is capped at 512 MiB total and 96 MiB per physical line, then
    /// spooled to a validated temporary snapshot before writes. Dispatch chunks
    /// are capped at 100 ops and 32 MiB (one larger op runs alone). Progress is
    /// printed per chunk to stderr; the final aggregate summary is printed to
    /// stdout, or `--save-file` incrementally writes ordered JSONL rows to a
    /// sibling temp file. Success atomically publishes the complete file and
    /// prints its ordinary manifest. A later failure leaves database effects
    /// incremental, discards the incomplete temp file, and prints an aborted
    /// reconciliation manifest before returning non-zero.
    /// Pass `--serial` to retain those logical chunks while awaiting every
    /// handler before starting the next; this keeps one warm server/model and
    /// limits handler concurrency to one.
    ///
    /// Mutually exclusive with the positional `ops` argument.
    #[arg(long, value_name = "PATH")]
    pub ops_file: Option<PathBuf>,

    /// Parse and validate every op, print the would-be summary, then exit
    /// without writing anything.  Only valid with `--ops-file`.
    #[arg(long, requires = "ops_file")]
    pub dry_run: bool,

    /// Await each JSON op to completion before dispatching the next one.
    ///
    /// This keeps one in-process server and its loaded models warm while
    /// limiting handler concurrency to exactly one. It is intended for
    /// resource-constrained backends where an ordinary parallel ops-file batch
    /// can exhaust a reader or model resource. Only valid with `--ops-file` and
    /// mutually exclusive with both positional inline ops and `--atomic`.
    #[arg(
        long,
        requires = "ops_file",
        conflicts_with_all = ["atomic", "ops"]
    )]
    pub serial: bool,

    /// Run the whole ops-file as ONE cross-op atomic unit (ADR-099): every op
    /// commits or the whole file rolls back, with zero partial state either
    /// way. Only valid with `--ops-file`. Only the v1 admissible verb set
    /// (`update`, `delete`, `link`, `merge`, `gtd.transition`, `gtd.complete`)
    /// may appear in the file — an embedding-bearing verb (`create`, ...), a
    /// read verb, or an unlisted verb is rejected before any write. Without
    /// this flag, `--ops-file` behavior is unchanged (chunked, best-effort,
    /// per-op success/failure).
    #[arg(long, requires = "ops_file")]
    pub atomic: bool,

    /// Maximum op count admitted into one `--atomic` unit (ADR-099 D2 defers
    /// the exact threshold to harness measurement; see
    /// `khive_types::pack::ATOMIC_MAX_OPS_DEFAULT` for the interim default
    /// and its rationale). Rejected before any write when exceeded. Only
    /// meaningful with `--atomic`.
    #[arg(long, requires = "atomic")]
    pub atomic_max_ops: Option<usize>,

    /// Exit non-zero when any op in the batch fails (or, for `--ops-file`,
    /// when any applied op fails). Without this flag a *partially* failed
    /// batch still exits 0 — the per-op `results` entries and the
    /// `summary`/`status` fields in the printed output are the signal
    /// (#1220). A batch in which *every* op failed always exits non-zero,
    /// with or without this flag (#1339). A rolled-back `--atomic` unit also
    /// exits non-zero, while a durable `committed_degraded` unit exits zero.
    /// Under `--atomic`, this flag additionally annotates otherwise-unclassified
    /// not-committed result rows with the stable `strict-op-failure` reason.
    #[arg(long)]
    pub strict: bool,
}
