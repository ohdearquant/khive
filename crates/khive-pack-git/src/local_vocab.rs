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
    description: "Compare commits or khive tree manifests through diff-tree with external diff, text conversion, color and rename detection disabled. Returns the diff blob, input pair, summary and receipt_id.",
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
    description: "Read effective repository allowlist rows, preserving their configured entry indices and branch patterns.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[REPO],
};
pub(crate) const RECONCILE: HandlerDef = HandlerDef {
    name: "git.reconcile",
    description: "Settle a caller-owned unknown local receipt only when its receipt marker and SHA appear in the ref reflog and that SHA is the current head or an ancestor. Missing evidence leaves unknown. Never retries a repository write or accesses a remote.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[param("receipt", true, "Caller-owned receipt UUID of a local branch or tree commit operation.")],
};
