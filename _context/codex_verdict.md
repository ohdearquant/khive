Verdict: APPROVE-WITH-FIXES
Findings: 0 Blocker, 0 High, 1 Medium, 1 Low

Scope note: GitHub PR #605 currently points at `bb239e0`; this local worktree is one commit ahead at
`f97f87b`, which adds `_context/status.md`. I reviewed the GitHub PR code at `bb239e0` and the
local status artifact where it is the only available #533 skip-rationale document. `_context/status.md`
changed during this review; I did not revert that concurrent update.

### [Medium] Warm Hook Has No Direct Regression Test Despite The PR Test Claim

Evidence: `crates/khive-pack-kg/src/lib.rs:816` adds `KgPack::warm()` and `crates/khive-pack-kg/src/lib.rs:819`
performs the sentinel `runtime.embed("khive warmup")`; the added package integration coverage at
`crates/khive-pack-kg/tests/integration.rs:829` covers only the #518 tag-filtered search path.
`rg "kg_pack_warm_invokes_default_embedder|warm_invokes|khive warmup"` found only the implementation,
not the PR-body test named `kg_pack_warm_invokes_default_embedder`.

Why this matters: #551 is behavior-changing startup logic. The implementation is small and plausible,
but without a mock embedder/spy test, a future no-op warm implementation, lost `tokio::spawn`, or wrong
runtime call would still pass the current package tests. The PR description also overclaims test
coverage that is not present in the actual diff.

Suggested fix: Add a focused warm test using a mock/default embedder provider that records one embed
call after `KgPack::warm()`, or remove the named test claim from the PR body.

### [Low] #533 Skip Rationale Is Not Packaged With The Current GitHub PR Head

Evidence: `_context/status.md:6` contains the useful #533 skip rationale: live `AnnBridge` has no
namespace/model/fingerprint/generation metadata and `kkernel` cannot clear in-process `SharedAnn`.
The current GitHub PR head `bb239e0` changes only the four `crates/khive-pack-kg` files, while the
status artifact is in the local-only `f97f87b` commit. `_context/status.md:6` also points to
`fix_brief.md`, but no `fix_brief.md` was present in or above this worktree.

Why this matters: The skip decision is technically defensible: `crates/khive-pack-knowledge/src/knowledge/vamana.rs:20`
shows `AnnBridge` only stores the index and ID map, `crates/khive-pack-knowledge/src/knowledge/vamana.rs:460`
returns immediately when any ANN is loaded, and `crates/kkernel/src/reindex.rs:309` only invalidates persisted
snapshots. But the PR artifact should carry that rationale where reviewers can see it.

Suggested fix: Push the local status commit, inline the #533 rationale in the PR body, or remove the
missing `fix_brief.md` reference and make `_context/status.md` the canonical rationale.

## Looks Right

- #518 tag filtering is in the requested place: `crates/khive-pack-kg/src/handlers.rs:2472` runs hybrid
  search first, `crates/khive-pack-kg/src/handlers.rs:2487` fetches entity metadata for the candidate
  IDs, and `crates/khive-pack-kg/src/handlers.rs:2515` applies properties/tags as post-filters before
  result shaping.
- #518 semantics match the request: `crates/khive-pack-kg/src/handlers.rs:1079` implements OR matching
  with case-insensitive comparison, and `crates/khive-pack-kg/src/lib.rs:472` documents the new `tags`
  search parameter.
- #551 implementation follows ADR-049's best-effort warm model: `crates/khive-runtime/src/pack.rs:1077`
  fans out pack warm hooks, `docs/adr/ADR-049-khived-daemon.md:75` says warm is non-blocking, and
  `crates/khive-pack-kg/src/lib.rs:816` schedules the sentinel embed without propagating errors.
- #533 skip rationale is directionally correct for this PR's scope; fixing live ANN coherence spans
  knowledge-pack ANN metadata, persisted generation/fingerprint state, and `kkernel` daemon/process
  invalidation behavior.

## Commands Run

- `git status --short --branch`: worktree on `show/khive-issue-sweep/retrieval-fixes`; `_context/issues.json`
  and `_context/triage.md` were untracked at review start.
- GitHub PR metadata via connector: PR #605 is open, head `bb239e0`, 4 changed files.
- `git log --oneline --decorate --graph --max-count=8 --all`: confirmed local `f97f87b` is one commit ahead
  of GitHub PR head `bb239e0`.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml -p khive-pack-kg tags -- --nocapture`:
  passed, 5 unit tests and 1 integration test.
- `RUSTC_WRAPPER= cargo check --manifest-path crates/Cargo.toml -p khive-pack-kg`: passed.
- `RUSTC_WRAPPER= cargo clippy --manifest-path crates/Cargo.toml -p khive-pack-kg --all-targets -- -D warnings`:
  passed.
- Initial `cargo test --manifest-path crates/Cargo.toml -p khive-pack-kg tags -- --nocapture` without
  `RUSTC_WRAPPER=` failed because `sccache` was not permitted in the sandbox.

## What I Did Not Check

- I did not run full `cargo test --workspace` or full workspace clippy under the deadline.
- I did not exercise a real daemon startup path or measure embedding cold-start latency.
- I did not post this review to GitHub.

## Re-Review Guidance

Narrow re-review is enough: check that the warm test or PR-body correction was added, and verify that
the #533 skip rationale is visible in the actual PR artifact.

Domain utility: SKIPPED — the khive memory/knowledge recall call was unavailable in this run, so the
review used the repository ADRs, code, local context artifacts, and targeted tests directly.
