# IMPL_REPORT — ADR-002 provenance amendment

## Scope

Amended ADR-002 (Closed Edge Ontology) with four new base-contract endpoint pairs and
mirrored the change in the runtime's base endpoint allowlist (`BASE_ENTITY_ENDPOINT_RULES`
in `crates/khive-runtime/src/operations.rs`) — not the pack-kg `EDGE_RULES` extension array.

Rationale for base-contract placement over a pack extension: `introduced_by` and
`depends_on` already have rows in ADR-002's own base contract tables (Derivation and
Dependency relations respectively), and the amendment is a base-ontology completion
(document authorship, concept origination by org, document-to-document normative
dependency) rather than a pack-specific research-KG stretch like the existing
`person→org`/`org→org` `KG_EDGE_RULES` additions. The prior epistemic-category amendment
(ADR-055, `supports`/`refutes`) set this precedent: it landed directly in
`BASE_ENTITY_ENDPOINT_RULES`, not in a pack.

## Pairs added

| Source     | Relation        | Target     |
| ---------- | --------------- | ---------- |
| `Document` | `introduced_by` | `Person`   |
| `Document` | `introduced_by` | `Org`      |
| `Concept`  | `introduced_by` | `Org`      |
| `Document` | `depends_on`    | `Document` |

All four are additive only — no existing row was removed or narrowed.

## Files touched

- `docs/adr/ADR-002-edge-ontology.md` — amendment header note (dated 2026-07-08), four new
  rows in the "Derivation relations" and "Dependency relations" base-contract tables, two
  inline amendment callouts explaining the rationale for each table, and a new Rationale
  subsection "Why the 2026-07-08 provenance amendment?" preceding "Why 9 categories?".
- `crates/khive-runtime/src/operations.rs` —
  - `BASE_ENTITY_ENDPOINT_RULES`: added the four rows (three under Derivation/`IntroducedBy`,
    one under Dependency/`DependsOn`), mirroring the existing array's per-category grouping
    and comment style.
  - `operations::tests`: five new `#[tokio::test]` cases at the end of the module —
    `link_document_introduced_by_person_allowed`, `link_document_introduced_by_org_allowed`,
    `link_concept_introduced_by_org_allowed`, `link_document_depends_on_document_allowed`
    (positive, one per new pair, via `rt.link(...)`), and
    `link_org_introduced_by_document_rejected_direction_matters` (negative guard — confirms
    the reverse direction `Org introduced_by Document` remains rejected, since only
    `Document introduced_by Org` was added).

No changes were needed in `crates/khive-pack-kg/src/pack.rs` (`KG_EDGE_RULES`,
`kg_pack_edge_rules_cover_expected_relations` test) — that array and its tripwire test cover
only the pack's own additive rules (person/org membership semantics), which this amendment
does not touch.

## Consumer-ADR grep results

`grep -rlE "introduced_by|depends_on" docs/adr/*.md AGENTS.md` matched 17 files. Inspected
every match for tables that *restate the full endpoint pair set* for `introduced_by` or
`depends_on` (the link-referential-integrity failure mode) rather than merely mentioning the
relation name or documenting an unrelated, independently-scoped pair:

- `ADR-001-entity-kind-taxonomy.md` — has an "Edge endpoint rules for new kinds" section with
  its own `Artifact`/`Service` tables (`Artifact introduced_by Document`,
  `Artifact depends_on Project/Service`, `Service depends_on Project/Service/Artifact/Dataset`).
  This section documents rules for the two entity kinds ADR-001 introduced, not an exhaustive
  restatement of ADR-002's full `introduced_by`/`depends_on` set — none of the rows it lists
  are affected by this amendment. No change needed.
- `ADR-017-pack-standard.md` — one row, `depends_on: task → task` (GTD pack, ADR-019). Unrelated
  pair, unaffected.
- `ADR-019-gtd-pack.md` — `depends_on` used as a task property/edge design discussion, not an
  endpoint-pair table.
- `ADR-036-kg-import-export-adapters.md` — maps Markdown section headers to `depends_on`,
  unrelated to endpoint kind pairs.
- `ADR-047-knowledge-pack.md` — references `introduced_by` as the target relation for
  `knowledge.cite`, no pair enumeration.
- `ADR-048-knowledge-section-profiles.md` — a lint rule name (`missing-introduced-by`), no pair
  table.
- `ADR-055-epistemic-edge-relations.md` — one descriptive row contrasting `introduced_by`
  semantics with `supports`/`refutes`; no pair enumeration.
- `ADR-085-code-pack.md` — `depends_on` rows scoped entirely to `concept/{function,datatype,
  interface,module}` typed pairs (khive-pack-code's own `EntityOfType` rules). Independently
  scoped, unaffected.
- `ADR-069`, `ADR-072`, `ADR-074`, `ADR-076`, `ADR-084`, `ADR-088`, `ADR-095`, `README.md` —
  matched only in prose (no tabular endpoint-pair restatement) or not at all after the `|.*|`
  filter.
- `AGENTS.md` — `introduced_by` appears in the `knowledge.cite` verb table and the per-kind
  edge-density guidance table (`concept`/`person` density minimums referencing `introduced_by`
  as a preferred relation, not an endpoint-pair enumeration), plus a "common mistakes" row
  about `introduced_by` direction. No endpoint-pair table to update. This file lives in the
  khive repo root and is in scope; it required no change. The root-level
  `/Users/lion/projects/CLAUDE.md` §KG legend (outside this worktree, per instructions) was
  not touched — flagging it here as a possible follow-up if that legend is ever found to
  restate specific endpoint pairs (from what's visible in this session's system context, it
  documents relation *categories*, not per-pair tables, so likely does not need a follow-up,
  but it was not directly inspected from this worktree).

Conclusion: no other in-repo document required updating. The amendment is additive and
orthogonal to every other endpoint-pair table found in the repo.

## Publication-hygiene self-audit

```
grep -riE "lambda:|atlas|leo|ocean|codex" docs/adr/ADR-002-edge-ontology.md
```

No output — clean.

## Gate results (all run from `crates/`)

| Gate | Command | RC |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | 0 (after running `cargo fmt --all` once to apply the one multi-line `.link(...)` call reflow) |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | 0 |
| test (khive-pack-kg) | `cargo test -p khive-pack-kg` | 0 — 212 + 11 passed |
| test (khive-runtime) | `cargo test -p khive-runtime` | 0 — 694 passed, 5 ignored (pre-existing, unrelated), including all 5 new tests |
| certificate (ADR-076 endpoint-signature tripwire) | `cargo test -p khive-pack-kg --test certificate` | 0 — 3 passed, no new signature collisions introduced |
| check --workspace | `cargo check --workspace` | 0 |
| doc -D warnings | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p khive-runtime -p khive-pack-kg` | 0 |
| deno fmt --check (ADR) | `deno fmt docs/adr/ADR-002-edge-ontology.md` | applied cleanly (no diff on second run) |

## Not done / explicitly out of scope

- No change to `KG_EDGE_RULES` in `khive-pack-kg/src/pack.rs` (not applicable — see above).
- No change outside this worktree (root `CLAUDE.md` KG legend) per task instructions; flagged
  above as a possible follow-up, not verified from this worktree.
- Not pushed. One commit only, in the worktree's local branch
  `feat/adr002-provenance-amendment`.
