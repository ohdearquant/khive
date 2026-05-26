Verdict: REJECT
Findings: 1 Blocker, 2 High, 1 Medium, 0 Low

PR #463 local head verified: `742f2b1ccf50dbe204c9eaa471bee9f512b07852` on base `8837f234fbb11e6b5767d83cdc4d0aad56247abd`.

## Findings

### [Blocker] `get(id=<proposal_id>)` Returns the Projection Row, Not the ProposalCreated Payload

Evidence: `docs/adr/ADR-046-event-sourced-proposals.md:299` says `get(id=<proposal_id>)` resolves to the `ProposalCreated` event payload, and `docs/adr/ADR-046-event-sourced-proposals.md:497`-`498` distinguishes `list(kind=proposal, status="open")` as the projection-table browse surface from `get(id=<proposal_id>)` as the event-payload fetch. The implementation acknowledges that contract at `crates/khive-pack-kg/src/handlers.rs:1114`-`1117`, but `try_get_proposal_row` queries only `proposals_open` projection columns at `crates/khive-pack-kg/src/handlers.rs:1139`-`1153` and returns only those aggregate/projection fields at `crates/khive-pack-kg/src/handlers.rs:1214`-`1227`.

Why this matters: C3 claims to fix ADR-046 `get` resolution, but it returns the wrong object. Callers still cannot recover the actual proposed change because the response omits `description`, `changeset`, `reviewers`, and `parent_id`; it is just another projection-row read.

Suggested fix: Resolve the proposal id against `proposals_open` only to disambiguate the proposal id, then query the event log for `EventKind::ProposalCreated` with `payload_proposal_id` and return the `ProposalCreatedPayload`. Add a regression test that asserts `get(id=<proposal_id>)` includes the serialized `changeset`.

### [High] H2 Documents and Preserves JSON-String Changesets Instead of ADR-046 Structured Drafts

Evidence: ADR-046 defines structured changeset arms: `AddEntity { entity: EntityDraft }`, `UpdateEntity { id: Uuid, patch: EntityPatch }`, and `AddNote { note: NoteDraft }` at `docs/adr/ADR-046-event-sourced-proposals.md:96`-`106`. The public type instead stores those payloads as `String` at `crates/khive-types/src/event.rs:291`-`305`, and this PR's help text makes that stringly surface official by documenting `entity: <JSON-string ...>`, `patch: <JSON-string ...>`, and `note: <JSON-string ...>` at `crates/khive-pack-kg/src/lib.rs:407`-`415`.

Why this matters: Proposal events are supposed to serialize as typed proposal payloads, not as JSON embedded inside JSON strings. This keeps validation and schema clarity outside the event substrate and makes downstream consumers parse nested strings to understand a proposal.

Suggested fix: Introduce structured draft/patch types for the proposal event payload, or amend ADR-046 before documenting the string-encoded wire shape. The MCP help should not publish an ADR-incompatible schema.

### [High] `list(kind=proposal)` Now Silently Hides Retained Audit Rows

Evidence: ADR-046 says hard-state proposal rows are retained for audit at `docs/adr/ADR-046-event-sourced-proposals.md:277`-`279`, and says `list(kind=proposal)` supports standard filters at `docs/adr/ADR-046-event-sourced-proposals.md:501`-`504`. This PR changes the no-status path to add `status IN ('open', 'changes_requested')` at `crates/khive-pack-kg/src/handlers.rs:2408`-`2420`, and invents `status="all"` as an escape hatch at `crates/khive-pack-kg/src/handlers.rs:2422`-`2424`.

Why this matters: A standard `list` without a status filter should not silently drop `approved`, `rejected`, `applied`, and `withdrawn` rows that the ADR explicitly retains for audit. The new `status="all"` sentinel is not part of the ADR surface and changes existing/default behavior without a contract update.

Suggested fix: Keep `list(kind=proposal)` unfiltered unless `status` is supplied, or update ADR-046 and the MCP schema to define the actionable-default behavior and the `all` sentinel.

### [Medium] Rustfmt Gate Fails on Touched Files

Evidence: `cargo fmt --all -- --check` reports diffs in touched files, including the long assertion at `crates/khive-pack-kg/src/apply_worker.rs:746`, the wrapped `try_get_proposal_row` call/signature at `crates/khive-pack-kg/src/handlers.rs:1073`-`1075` and `crates/khive-pack-kg/src/handlers.rs:1129`-`1133`, and vector literals at `crates/khive-pack-kg/src/handlers.rs:1143`-`1146`, `crates/khive-pack-kg/src/handlers.rs:1155`-`1158`, and `crates/khive-pack-kg/src/handlers.rs:2021`-`2024`.

Why this matters: `CLAUDE.md` requires `cargo fmt --all -- --check` with no exceptions. The PR cannot be accepted while a repository-standard formatting gate fails.

Suggested fix: Run `cargo fmt --all` from `crates/` and include the formatted diff.

## Looks Right

- `Id128::deserialize` now deserializes through an owned `String` at `crates/khive-types/src/id.rs:165`-`170`, which is the correct fix for `serde_json::Value`-backed deserializers.
- The new Id128/proposal changeset regression tests ran in `cargo test --workspace`; `id::tests::deserialize_from_owned_value` and `event::tests::proposal_changeset_id_variants_deserialize_from_value` both passed.
- The proposal verbs remain kg-substrate verbs, not pack-prefixed: `propose`, `review`, and `withdraw` are exposed directly at `crates/khive-pack-kg/src/lib.rs:384`-`485`.
- `review`/`withdraw` now route through a proposal-prefix resolver and return an explicit ambiguity error for multiple prefix matches at `crates/khive-pack-kg/src/handlers.rs:2001`-`2057`.

## Commands Run

- `date -Iseconds`: `2026-05-25T22:49:18-04:00`.
- `git status --short --branch`: clean branch `v025-w4-i-proposal...origin/v025-w4-i-proposal`.
- `git diff main..HEAD --stat`: 5 files changed, 393 insertions, 40 deletions.
- GitHub PR metadata fetch for `ohdearquant/khive#463`: PR is open draft, 1 commit, base `8837f234...`, head `742f2b1...`.
- `git rev-parse HEAD`: `742f2b1ccf50dbe204c9eaa471bee9f512b07852`.
- `git rev-parse main`: `8837f234fbb11e6b5767d83cdc4d0aad56247abd`.
- `cargo test --workspace` from repository root: failed immediately because the repo root has no `Cargo.toml`.
- `cargo test --workspace` from `crates/`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings` from `crates/`: failed before compilation because `sccache` is not permitted in this sandbox.
- `RUSTC_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings` from `crates/`: passed.
- `RUSTC_WRAPPER= cargo fmt --all -- --check` from `crates/`: failed with rustfmt diffs in `khive-pack-kg/src/apply_worker.rs` and `khive-pack-kg/src/handlers.rs`.

## What I Did Not Check

- I did not run `cargo build --release --bin khive-mcp`; it was claimed in the PR description but not requested in this review instruction.
- I did not exercise the MCP server end-to-end through stdio; review was source, ADR, and Rust gate based.

## Re-Review Guidance

Broad re-review is needed after fixes. The `get(id=<proposal_id>)` shape and changeset schema are public contract issues, not formatting-only fixes.

Domain utility: SKIPPED - lore `suggest`/`compose` tools were not available in this Codex environment; I used the khive PR review skill plus the accepted ADR corpus.
