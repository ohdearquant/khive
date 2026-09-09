//! Vocabulary, schema plan and handler table for the exec pack.

use khive_types::{HandlerDef, IdResolutionMode, ParamDef, VerbCategory, Visibility};

/// Canonical pack name; every verb is `exec.<name>`.
pub const PACK_NAME: &str = "exec";

/// Manifest schema tag carried by every tree object.
pub const TREE_SCHEMA: &str = "khive-tree/v1";

/// Pack-owned tables, applied idempotently at boot.
pub static EXEC_SCHEMA_PLAN_STMTS: [&str; 6] = [
    "CREATE TABLE IF NOT EXISTS exec_runs (\
        id           TEXT PRIMARY KEY,\
        namespace    TEXT NOT NULL,\
        actor        TEXT NOT NULL,\
        tool         TEXT NOT NULL,\
        session_id   TEXT,\
        seq          INTEGER,\
        receipt      TEXT NOT NULL,\
        created_at   INTEGER NOT NULL\
    )",
    "CREATE INDEX IF NOT EXISTS idx_exec_runs_actor ON exec_runs(namespace, actor, created_at)",
    "DROP INDEX IF EXISTS idx_exec_runs_session",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_exec_runs_session_seq \
        ON exec_runs(namespace, session_id, seq) WHERE session_id IS NOT NULL",
    "CREATE TABLE IF NOT EXISTS exec_events (\
        id         INTEGER PRIMARY KEY AUTOINCREMENT,\
        namespace  TEXT NOT NULL,\
        run_id     TEXT NOT NULL,\
        kind       TEXT NOT NULL,\
        at         INTEGER NOT NULL,\
        detail     TEXT\
    )",
    "CREATE INDEX IF NOT EXISTS idx_exec_events_run ON exec_events(namespace, run_id)",
];

const P_NAMESPACE: ParamDef = ParamDef {
    name: "namespace",
    param_type: "string",
    required: false,
    description: "Namespace override (defaults to the caller's namespace).",
    resolution_mode: IdResolutionMode::NotApplicable,
};

const P_TREE: ParamDef = ParamDef {
    name: "tree",
    param_type: "string",
    required: true,
    description: "Tree manifest reference (BLAKE3 blob ref of a khive-tree/v1 manifest).",
    resolution_mode: IdResolutionMode::NotApplicable,
};

#[rustfmt::skip]
pub static EXEC_HANDLERS: [HandlerDef; 9] = [
    HandlerDef {
        name: "exec.tree",
        description: "Store a tree manifest from entries [{path, ref, mode}] and return its reference. Paths are relative and normalized, modes are 644 or 755, duplicates and symlinks are refused.",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef { name: "entries", param_type: "array", required: true, description: "Entries [{path, ref, mode}]; an empty array is the empty tree.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "exec.tree_get",
        description: "Read a tree manifest back as its entries.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[P_TREE, P_NAMESPACE],
    },
    HandlerDef {
        name: "exec.tree_put",
        description: "Apply edits [{path, ref|content|delete, mode}] to a tree and return the new tree. Trees are immutable, so this mints a new manifest and never changes the input. One call yields exactly one new tree or none: any refusal stores nothing, including blobs for entries that were fine. Duplicate paths, an empty edits list, and a delete of a path the tree does not hold are refused.",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            P_TREE,
            ParamDef { name: "edits", param_type: "array", required: true, description: "Edits [{path, and exactly one of ref | content | delete:true, plus optional mode}]; an empty array is refused.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "exec.tree_diff",
        description: "Compare two tree manifests: changed [{path, op: added|modified|deleted, ref, base_ref}].",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef { name: "base", param_type: "string", required: true, description: "Base tree reference.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "head", param_type: "string", required: true, description: "Head tree reference.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "exec.run",
        description: "Run one registered tool over a materialized tree inside the seatbelt sandbox under tool.check; returns {receipt, changed}. Every refusal writes a receipt and names it in the error as receipt_id=<id>.",
        visibility: Visibility::Verb,
        category: VerbCategory::Directive,
        params: &[
            P_TREE,
            ParamDef { name: "tool", param_type: "string", required: true, description: "Registered tool name (kind tool, source exec:<absolute path>).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "args", param_type: "array", required: false, description: "Arguments after the binary.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "actor", param_type: "string", required: true, description: "Calling actor label; must match the authenticated caller.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "cwd", param_type: "string", required: false, description: "Working directory relative to the tree root (default '.').", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "env", param_type: "object", required: false, description: "Caller environment; only keys allow-listed by [exec] env pass through.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "timeout_s", param_type: "number", required: false, description: "Wall-clock limit in seconds (default and ceiling from [exec]).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "session_id", param_type: "string", required: false, description: "Session label; receipts carry a per-session sequence.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "declared_write_paths", param_type: "array", required: false, description: "Paths the run may change; undeclared changes are dropped and the run reports success:false.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "exec.receipt",
        description: "Read one run receipt by id.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef { name: "id", param_type: "string", required: true, description: "Receipt id.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "exec.runs",
        description: "List run receipts for an actor, newest first, optionally filtered by tool and session.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef { name: "actor", param_type: "string", required: true, description: "Actor label.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "tool", param_type: "string", required: false, description: "Tool name filter.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "session_id", param_type: "string", required: false, description: "Session filter.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "limit", param_type: "integer", required: false, description: "Page size (default 20, max 500).", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "exec.events",
        description: "Append-only materialization and launch audit rows, oldest first.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef { name: "run_id", param_type: "string", required: false, description: "Only events of this run.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "limit", param_type: "integer", required: false, description: "Page size (default 200, max 5000).", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "exec.identity",
        description: "Effective exec configuration identity: resolved read roots and their digest, the profile template digest, limits and caps.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
];
