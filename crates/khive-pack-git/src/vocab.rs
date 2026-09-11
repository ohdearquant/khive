//! Git pack vocabulary: note kind specs, the `git.digest` handler
//! declaration, the `precedes` commit→commit edge extension, and the
//! pack-auxiliary cursor schema.
//!
//! See crates/khive-pack-git/docs/api/vocab.md for the ADR-088 v0 → Amendment 1
//! rationale behind this module's design.

use khive_runtime::{NoteKindSpec, NoteLifecycleSpec};
use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, EntityKind, EntityTypeDef, HandlerDef,
    IdResolutionMode, ParamDef, VerbCategory, Visibility,
};

/// Shared open/closed lifecycle for `issue` and `pull_request`. See
/// crates/khive-pack-git/docs/api/vocab.md#git_lifecycle.
const GIT_LIFECYCLE: NoteLifecycleSpec = NoteLifecycleSpec {
    field: "kind_status",
    initial: "open",
    terminal: &["closed"],
    transitions: &[("open", "closed"), ("closed", "open")],
};

/// Note kind specs for the two lifecycle-bearing kinds this pack contributes.
///
/// `commit` deliberately has no entry: commits are immutable and carry no
/// lifecycle field.
pub(crate) static GIT_NOTE_KIND_SPECS: [NoteKindSpec; 2] = [
    NoteKindSpec {
        kind: "issue",
        aliases: &[],
        lifecycle: GIT_LIFECYCLE,
    },
    NoteKindSpec {
        kind: "pull_request",
        aliases: &[],
        lifecycle: GIT_LIFECYCLE,
    },
];

/// Pack-auxiliary schema: the git-ingest cursor table (ADR-088 §5). See
/// crates/khive-pack-git/docs/api/vocab.md#git_schema_plan_stmts.
pub(crate) static GIT_SCHEMA_PLAN_STMTS: [&str; 5] = [
    crate::receipts::RECEIPTS_TABLE_SQL,
    crate::receipts::RECEIPTS_ACTOR_INDEX_SQL,
    crate::receipts::RECEIPTS_SESSION_INDEX_SQL,
    "CREATE TABLE IF NOT EXISTS git_mirror_cursor (\
        project_id   TEXT NOT NULL,\
        kind         TEXT NOT NULL,\
        cursor_value TEXT,\
        updated_at   INTEGER NOT NULL,\
        PRIMARY KEY (project_id, kind)\
    )",
    "CREATE INDEX IF NOT EXISTS idx_git_mirror_cursor_updated \
        ON git_mirror_cursor(updated_at DESC)",
];

/// ADR-088 Amendment 1: parent→child commit lineage as `precedes` edges
/// (note→note extension). See crates/khive-pack-git/docs/api/vocab.md#git_edge_rules.
pub(crate) static GIT_EDGE_RULES: [EdgeEndpointRule; 1] = [EdgeEndpointRule {
    relation: EdgeRelation::Precedes,
    source: EndpointKind::NoteOfKind("commit"),
    target: EndpointKind::NoteOfKind("commit"),
}];

/// Pack-declared `Document` entity-type subtype: Architecture Decision
/// Records. See crates/khive-pack-git/docs/api/vocab.md#git_entity_types.
pub(crate) static GIT_ENTITY_TYPES: [EntityTypeDef; 1] = [EntityTypeDef {
    kind: EntityKind::Document,
    type_name: "adr",
    aliases: &["architecture_decision_record", "decision_record"],
}];

/// Illocutionary classification (Searle 1976): `git.digest` commits data to
/// the graph (ingests notes and edges), so it is `Commissive` — the same
/// category `create`/`link`/`remember` use. `git.commit` / `git.branch` /
/// `git.push` (ADR-108) mutate a git repository, not the graph, but are
/// still `Commissive` — the speaker commits a persistent change, exactly the
/// same illocutionary force as `create`/`link`, just against a different
/// substrate (a git repo instead of khive's own storage).
pub(crate) static GIT_HANDLERS: [HandlerDef; 16] = [
    crate::local_vocab::INIT,
    crate::local_vocab::CHECKOUT,
    crate::local_vocab::DIFF,
    crate::local_vocab::RECEIPTS,
    crate::local_vocab::GATES,
    crate::local_vocab::RECONCILE,
    crate::local_vocab::STATUS,
    crate::local_vocab::LOG,
    HandlerDef {
        name: "git.ingest_cursor",
        description: "Read the stored ingest cursor and checkpoint for a project and source kind in one snapshot. Values are exact opaque strings, not a completion receipt or a guarantee of resumability; oversized values are explicitly omitted. No ingest, remote access, or cursor writes.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "project",
                param_type: "uuid",
                required: true,
                description: "Full UUID of the live project anchor. Canonical get authorization applies with the caller's identity; by-ID reads are namespace-agnostic.",
                resolution_mode: IdResolutionMode::UnscopedFullUuidOnly,
            },
            ParamDef {
                name: "source_kind",
                param_type: "string",
                required: true,
                description: "One of commits, issues, pull_requests. Commits use a SHA cursor; issues and pull_requests use timestamp cursors with page checkpoints.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "git.digest",
        description: "Ingest commit/issue/pull_request provenance from a local git repo path or \
                       an https:// URL into the graph. Bounded and cursor-resumable: call \
                       repeatedly until the response's `done` field is true. Check \
                       `writes_refused` is zero before treating a completed pass as clean. Every \
                       successful response includes a durable audit-event `receipt_id`.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "source",
                param_type: "string",
                required: true,
                description: "Absolute local path to a git repository (must contain a .git \
                               entry), or an https:// URL. Any https host is accepted; \
                               non-github.com hosts degrade to commits-only (gh cannot serve \
                               their issues/PRs). ssh://, git://, http://, and scp-shorthand \
                               (user@host:path) sources are rejected.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "project",
                param_type: "string",
                required: false,
                description: "UUID or 8+ hex prefix of the repo-anchor project entity. When \
                               absent, resolved slug-first through properties.repo_slug with \
                               exact then normalized properties.repo_url reconciliation, or \
                               created if no identity evidence matches (see the response's \
                               project_id and project_created). Names are never a match key.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "max_items",
                param_type: "integer",
                required: false,
                description: "Bounded work for this call, counted across commits + issues + PRs \
                               (default 500, clamped to 1..=2000). Cursor-resumable: call again \
                               while the response's done field is false.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "include",
                param_type: "array of string",
                required: false,
                description: "Which record kinds to ingest this call: any of commits | issues | \
                               pull_requests (default: all three).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "git.commit",
        description: "Commit a complete tree manifest with actor identity and an atomic expected-head compare, or use the legacy paths form. Tree commits require the tool pack and actor mapping, preserve the index/worktree, and return receipt_id with the SHA. Paths retain their legacy behavior.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            crate::local_vocab::BRANCH,
            crate::local_vocab::TREE,
            crate::local_vocab::EXPECTED_HEAD,
            crate::local_vocab::SESSION,
            ParamDef {
                name: "repo",
                param_type: "string",
                required: true,
                description: "Absolute local path to a git repository (must contain a .git \
                               entry).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "message",
                param_type: "string",
                required: true,
                description: "Commit message, passed to git as a single -m argument value.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "paths",
                param_type: "array of string",
                required: false,
                description: "Relative paths to stage and scope the commit to. Absent commits \
                               everything currently staged/modified in tracked files (git \
                               commit -a) — never auto-adds new untracked files.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "author",
                param_type: "string",
                required: false,
                description: "Override the commit author, e.g. \"Name <email>\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "git.branch",
        description: "Create a branch from a ref or SHA (default HEAD) using a create-only atomic ref compare. Existing and symbolic target refs refuse. Requires the tool pack; returns legacy keys plus ref, sha and receipt_id.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            crate::local_vocab::EXPECTED,
            crate::local_vocab::SESSION,
            ParamDef {
                name: "repo",
                param_type: "string",
                required: true,
                description: "Absolute local path to a git repository (must contain a .git \
                               entry).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "name",
                param_type: "string",
                required: true,
                description: "New branch name.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "from",
                param_type: "string",
                required: false,
                description: "Ref or SHA to branch from. Absent uses the repo's current HEAD.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    crate::remote_vocab::PUSH,
    crate::remote_vocab::PR_OPEN,
    crate::remote_vocab::PR_REVIEW,
    crate::remote_vocab::PR_MERGE,
];
