//! Git pack vocabulary: note kind specs, the `git.digest` handler
//! declaration, the `precedes` commit→commit edge extension, and the
//! pack-auxiliary cursor schema.
//!
//! See crates/khive-pack-git/docs/vocab.md for the ADR-088 v0 → Amendment 1
//! rationale behind this module's design.

use khive_runtime::{NoteKindSpec, NoteLifecycleSpec};
use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, EntityKind, EntityTypeDef, HandlerDef, ParamDef,
    VerbCategory, Visibility,
};

/// Shared open/closed lifecycle for `issue` and `pull_request`. See
/// crates/khive-pack-git/docs/vocab.md#git_lifecycle.
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
/// crates/khive-pack-git/docs/vocab.md#git_schema_plan_stmts.
pub(crate) static GIT_SCHEMA_PLAN_STMTS: [&str; 2] = [
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
/// (note→note extension). See crates/khive-pack-git/docs/vocab.md#git_edge_rules.
pub(crate) static GIT_EDGE_RULES: [EdgeEndpointRule; 1] = [EdgeEndpointRule {
    relation: EdgeRelation::Precedes,
    source: EndpointKind::NoteOfKind("commit"),
    target: EndpointKind::NoteOfKind("commit"),
}];

/// Pack-declared `Document` entity-type subtype: Architecture Decision
/// Records. See crates/khive-pack-git/docs/vocab.md#git_entity_types.
pub(crate) static GIT_ENTITY_TYPES: [EntityTypeDef; 1] = [EntityTypeDef {
    kind: EntityKind::Document,
    type_name: "adr",
    aliases: &["architecture_decision_record", "decision_record"],
}];

/// Illocutionary classification (Searle 1976): `git.digest` commits data to
/// the graph (ingests notes and edges), so it is `Commissive` — the same
/// category `create`/`link`/`remember` use.
pub(crate) static GIT_HANDLERS: [HandlerDef; 1] = [HandlerDef {
    name: "git.digest",
    description: "Ingest commit/issue/pull_request provenance from a local git repo path or an \
                   https:// URL into the graph. Bounded and cursor-resumable: call repeatedly \
                   until the response's `done` field is true.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        ParamDef {
            name: "source",
            param_type: "string",
            required: true,
            description: "Absolute local path to a git repository (must contain a .git entry), \
                           or an https:// URL. Any https host is accepted; non-github.com hosts \
                           degrade to commits-only (gh cannot serve their issues/PRs). ssh://, \
                           git://, http://, and scp-shorthand (user@host:path) sources are \
                           rejected.",
        },
        ParamDef {
            name: "project",
            param_type: "string",
            required: false,
            description: "UUID or 8+ hex prefix of the repo-anchor project entity. When absent, \
                           resolved by matching properties.repo_url or name, or created if none \
                           is found (see the response's project_id and project_created).",
        },
        ParamDef {
            name: "max_items",
            param_type: "integer",
            required: false,
            description: "Bounded work for this call, counted across commits + issues + PRs \
                           (default 500, clamped to 1..=2000). Cursor-resumable: call again \
                           while the response's done field is false.",
        },
        ParamDef {
            name: "include",
            param_type: "array of string",
            required: false,
            description: "Which record kinds to ingest this call: any of commits | issues | \
                           pull_requests (default: all three).",
        },
    ],
}];
