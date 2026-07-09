//! Git pack vocabulary: note kind specs, the `git.digest` handler
//! declaration, the `precedes` commit→commit edge extension, and the
//! pack-auxiliary cursor schema.
//!
//! ADR-088 v0 shipped with no `HANDLERS` and no `EDGE_RULES`: zero new verbs,
//! relying exclusively on the base `annotates` contract (note -> any
//! substrate) for provenance edges. ADR-088 Amendment 1 adds exactly one
//! verb (`git.digest`) and one endpoint extension (`precedes` commit→commit,
//! for parent→child commit lineage — the base contract only allows
//! `precedes` between five entity kinds, never between notes; this pack
//! extends it the same additive way `khive-pack-gtd` extends `depends_on` to
//! task→task). See `crates/khive-pack-git/src/pack.rs`.

use khive_runtime::{NoteKindSpec, NoteLifecycleSpec};
use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, ParamDef, VerbCategory, Visibility,
};

/// Lifecycle declaration shared by `issue` and `pull_request` — both track an
/// open/closed state with the same posture as ADR-088's `finding` precedent:
/// declared for introspection, not yet enforced by the runtime (Phase 1).
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

/// Pack-auxiliary schema: the git-ingest cursor table (ADR-088 §5, ADR-087
/// operational pattern reused).
///
/// Shape is intentionally generic across git record kinds within a project —
/// `kind` distinguishes `commits` / `issues` / `prs` cursors so a follow-up
/// pack (e.g. a code-review pack) can reuse this exact table for its own
/// cursor rows without a schema change, keyed by its own `project_id`/`kind`
/// pair. Idempotent (`CREATE TABLE IF NOT EXISTS`), applied once at pack
/// registration time; not part of the core versioned migration chain.
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

/// ADR-088 Amendment 1 ingest enrichment: parent→child commit lineage as
/// `precedes` edges. The base endpoint contract only allows `precedes`
/// between five entity kinds (`document`, `dataset`, `artifact`, `service`,
/// `project` — see `khive-runtime::operations::BASE_ENTITY_ENDPOINT_RULES`);
/// it has no note→note case at all. This is the same additive-extension
/// mechanism `khive-pack-gtd` uses for `depends_on` task→task.
pub(crate) static GIT_EDGE_RULES: [EdgeEndpointRule; 1] = [EdgeEndpointRule {
    relation: EdgeRelation::Precedes,
    source: EndpointKind::NoteOfKind("commit"),
    target: EndpointKind::NoteOfKind("commit"),
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
