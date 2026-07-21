# `kkernel kg commit` — tier-2 change-set commit

**ADRs**: ADR-020 §5,
ADR-101 (change-set model),
ADR-102 (Amendment to ADR-020)

`kg commit` restores the `kg commit` verb ADR-020 §5 specified but never shipped, scoped
per ADR-102's amendment: this is the commit step for an already-staged ADR-101 change-set,
run against ADR-102's own local-only staged-change-set/snapshot repository (D6) — never the
project-repository-embedded `.khive/kg/` layout the other `kg` verbs operate on.

## What the command does

1. Parse the change-set NDJSON-delta file via `khive_changeset::from_ndjson` (fail-loud on
   any parse/schema error — malformed input never reaches step 2).
2. Project the change-set's `create`/`link` ops into synthetic `entities.ndjson` /
   `notes.ndjson` / `edges.ndjson` content and run a **subset** of the same rule pass
   `kkernel kg validate` uses against them (see "Commit-time validation scope" below). Any
   `error`-severity finding refuses the commit.
3. On a clean pass: `git add` the change-set file into the target repo and `git commit`,
   carrying the ADR-101 D4 provenance trailers. Refuses (fail-loud, before touching git) if
   the target repo has any configured remote (ADR-102 D6).

## Commit-time validation scope

A change-set is a **partial** view of the graph: most `link` ops target entities or notes
created by an *earlier*, already-committed change-set, not by this one. Rule classes that
assume a complete known-ID universe (`referential-integrity`, `dangling-refs`) would
therefore flag the overwhelming majority of ordinary edges as broken if run against this
change-set alone — a false-positive storm, not a real finding. Those two classes are **not
evaluated here**; they are deferred to stage time, where the producer/reviewer has (or can
obtain) full graph context, per ADR-102 D5's own framing of `dangling-refs` as an offline,
dataset-scoped check.

`edge-endpoint-types` and `edge-direction-conventions` do not need this exclusion: both
already skip any edge whose endpoint fails to resolve within the given NDJSON dataset (see
`validate::check_edge_endpoint_types`), so restricting them to this change-set's own
`create` ops degrades gracefully to "check what we can see" rather than false-flagging.

`update`, `delete`, and `merge` ops are not re-projected into the synthetic view: they patch
or remove records that already exist outside this change-set, so this command has no fresh
kind/name/relation data to check for them beyond what ADR-102 D2 already routes to tier-2
review by construction (`delete`, `merge`, and any edge-relation/weight change are *always*
tier-2). Re-validating already-reviewed preimage data offline here would not catch anything
new.

## `run_commit_time_rules` — exclusion is structural, not a post-hoc filter

The `dangling-refs`/`referential-integrity` exclusion (above) is implemented by calling
`validate::configurable_rule_checks_partial_view`, which never invokes the built-in
dangling-ref evaluator at all — it is **not** a post-hoc filter over the returned
`RuleResult`s by public id. A post-hoc `id == "dangling-refs"` filter would also swallow
the malformed-config error result `validate_severity` emits under that same id, and any
generic `[[rules]]` entry a rules author happens to name `"dangling-refs"` — both of which
must still fail the commit.

## No SQLite in this module

No SQLite handle is opened anywhere in `kg/commit.rs` (ADR-102 D5 topology guard) —
`validate::build_taxonomy` builds its registry with `db_path: None`, exactly as `kg
validate` already does, and every NDJSON read is a plain file read against the synthetic
projection or the change-set file itself.
