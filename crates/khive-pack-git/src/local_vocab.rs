//! Local object operations and receipt discovery.

use khive_types::{HandlerDef, IdResolutionMode, ParamDef, VerbCategory, Visibility};

const fn param(name: &'static str, required: bool, description: &'static str) -> ParamDef {
    ParamDef {
        name,
        param_type: "string",
        required,
        description,
        resolution_mode: IdResolutionMode::NotApplicable,
    }
}

pub(crate) const BRANCH: ParamDef = param("branch", false, "Target branch; required for a tree commit. Its head must equal expected_head at the atomic ref update.");
pub(crate) const TREE: ParamDef = param("tree", false, "Complete khive-tree/v1 manifest reference. Mutually exclusive with paths; author, actor and credential overrides are refused.");
pub(crate) const EXPECTED_HEAD: ParamDef = param("expected_head", false, "Required 40-hex parent SHA for a tree commit. Null is refused. Ref update compares the current head atomically against this value.");
pub(crate) const SESSION: ParamDef = param(
    "session_id",
    false,
    "Optional session label copied to the durable receipt.",
);
pub(crate) const EXPECTED: ParamDef = param("expected", false, "Optional 40-hex SHA that from must resolve to. Null is refused. The new branch must not exist regardless of this parameter.");
const REPO: ParamDef = param(
    "repo",
    true,
    "Absolute local repository path in the configured git allowlist.",
);

pub(crate) const CHECKOUT: HandlerDef = HandlerDef {
    name: "git.checkout",
    description: "Read one resolved commit into an immutable khive tree manifest without changing refs, index or working files. Symlinks and submodules refuse. Returns commit, tree and receipt_id.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[REPO, param("ref", true, "Commit SHA or ref resolved exactly once before reading its objects."), SESSION],
};
pub(crate) const DIFF: HandlerDef = HandlerDef {
    name: "git.diff",
    description: "Compare commits or khive tree manifests through diff-tree with external diff, text conversion, color and rename detection disabled. Tree entries support regular files (644/755) and symlinks (120000), whose blobs contain literal target bytes. Returns the diff blob, input pair, summary and receipt_id.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[REPO, param("input_kind", true, "Either commits or trees."), param("base", true, "Base commit/ref or tree manifest, according to input_kind."), param("head", true, "Head commit/ref or tree manifest, according to input_kind."), SESSION],
};
pub(crate) const RECEIPTS: HandlerDef = HandlerDef {
    name: "git.receipts",
    description: "List caller-owned durable git receipts in insertion order with stable offset pagination. An actor filter naming another caller refuses. Returns receipts and next_offset.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[
        param("repo", false, "Optional canonical repository filter."),
        param("actor", false, "Optional actor filter; must equal the authenticated caller."),
        SESSION,
        ParamDef { name: "limit", param_type: "integer", required: false, description: "Page size, 1 through 500; default 100.", resolution_mode: IdResolutionMode::NotApplicable },
        ParamDef { name: "offset", param_type: "integer", required: false, description: "Nonnegative insertion-order offset; default zero. Continue with next_offset until null.", resolution_mode: IdResolutionMode::NotApplicable },
    ],
};
pub(crate) const GATES: HandlerDef = HandlerDef {
    name: "git.gates",
    description: "Read effective repository allowlist rows, preserving their configured entry indices and branch patterns. target names the configured remote, slug, visibility and kind (local or platform); absent mappings return null and invalid mappings return kind=unavailable with a reason.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[REPO],
};
pub(crate) const RECONCILE: HandlerDef = HandlerDef {
    name: "git.reconcile",
    description: "Settle a caller-owned unknown receipt using observed evidence. Local receipts require the receipt marker and SHA in the ref reflog, with that SHA at the current head or an ancestor. Push receipts require an acknowledged local marker and exact remote SHA; explicitly local remote mappings read that SHA without resolving credentials; merge receipts read platform merged state and SHA. Missing evidence leaves unknown. Never repeats a write.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[param("receipt", true, "Caller-owned receipt UUID of a branch, tree commit, push or PR merge operation.")],
};

pub(crate) const STATUS: HandlerDef = HandlerDef {
    name: "git.status",
    description: "Read the working tree and index state as porcelain v2 without refreshing the index, taking a lock, or changing any file. Renames are not detected. Returns branch headers, a bounded entries page, and total, where total counts every entry git reported so total zero is a whole-repository claim. Takes no credential and writes no receipt.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[
        REPO,
        param("untracked", false, "Untracked file reporting: no, normal (default) or all."),
        ParamDef { name: "limit", param_type: "integer", required: false, description: "Entries returned, 1 through 5000; default 1000. total is never capped.", resolution_mode: IdResolutionMode::NotApplicable },
    ],
};
pub(crate) const LOG: HandlerDef = HandlerDef {
    name: "git.log",
    description: "Read a bounded page of commit history from one resolved ref, newest first, with decoration and color disabled and pathspecs taken literally. Returns sha, author name and email, authored_at, committed_at and subject per commit. Takes no credential and writes no receipt.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[
        REPO,
        param("ref", false, "Commit SHA or ref resolved exactly once; default HEAD."),
        ParamDef { name: "limit", param_type: "integer", required: false, description: "Commits returned, 1 through 500; default 100.", resolution_mode: IdResolutionMode::NotApplicable },
        param("path", false, "Optional literal path filter; never interpreted as a glob or magic pathspec."),
    ],
};

pub(crate) const INIT: HandlerDef = HandlerDef {
    name: "git.init",
    description: "Initialize an allowlisted directory that exists and holds no repository yet, with no template so it inherits no sample hooks. A target that already holds a repository is refused rather than reinitialized. The operator creates and allowlists the path; this verb never creates one. Returns repo, the resolved initial branch, and receipt_id.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        REPO,
        param("branch", false, "Initial branch name; default main."),
        SESSION,
    ],
};
