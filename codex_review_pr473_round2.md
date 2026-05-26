Verdict: APPROVE-WITH-SUGGESTIONS
Findings: 0 Blocker, 0 High, 0 Medium, 1 Low

## Findings

### [Low] Domain normalization only applies to the promoted tag

Evidence: `docs/adr/ADR-047-knowledge-pack.md:54` says `domain` is stored in `properties.domain` and appended to `tags`; `crates/khive-pack-knowledge/src/handlers.rs:115` stores `properties.domain` as `domain.trim()` while `crates/khive-pack-knowledge/src/handlers.rs:124` lowercases only the promoted tag. The response also returns the original caller value at `crates/khive-pack-knowledge/src/handlers.rs:149`.

Why this matters: The round-2 fix makes tag-based topic filtering work, but it can leave the two advertised domain surfaces with different casing for mixed-case input (`properties.domain = "Attention"`, `tags = ["attention"]`). That is not a blocker for `knowledge.topic`, because topic filters by tag, but it is a small data-shape inconsistency for callers using `properties.domain` for structured access.

Suggested fix: Normalize the trimmed domain once and use that value for `properties.domain`, the promoted tag, and the returned `domain`; or explicitly document that only the domain tag is normalized.

## Looks Right

- H1 is fixed at storage: `EntityFilter::tags_any` now lowercases query parameters and compares with `LOWER(json_each.value)` in SQL (`crates/khive-db/src/stores/entity.rs:231`-`245`).
- The listing path lowercases the requested domain before calling the DB-layer list/count helpers (`crates/khive-pack-knowledge/src/handlers.rs:216`-`221`, `crates/khive-pack-knowledge/src/handlers.rs:289`-`297`).
- The `learn` handler does lowercase the promoted domain tag at write time (`crates/khive-pack-knowledge/src/handlers.rs:119`-`127`), with the caveat above.
- H2 is resolved by making the search-path `total` contract honest: the doc comment now says search `total` is the post-filter count of the `limit * 4` candidate window, not a full corpus count (`crates/khive-pack-knowledge/src/handlers.rs:200`-`208`, `crates/khive-pack-knowledge/src/handlers.rs:223`-`284`).
- The listing path still reports a true pre-limit match count via `count_entities_tagged`, and the SQL count applies the same tag filter before the page limit (`crates/khive-pack-knowledge/src/handlers.rs:286`-`312`, `crates/khive-db/src/stores/entity.rs:420`-`456`, `crates/khive-db/src/stores/entity.rs:462`-`476`).
- Round-2 tests cover the topic listing case-insensitive path and the bounded search-path `total` semantics (`crates/khive-pack-knowledge/tests/integration.rs:330`-`420`).

## Commands Run

- `date -Iseconds`: confirmed round-2 review time.
- `git status --short --branch && git log --oneline -5`: confirmed commit `27e18e8` is HEAD; only prior `codex_review_pr473.md` was untracked before this review file.
- `git diff --stat main..HEAD` / `git diff --name-only main..HEAD`: confirmed the round-2 diff includes storage and knowledge-pack tests in addition to round-1 files.
- `git diff 1a3a10d..HEAD -- crates/khive-db/src/stores/entity.rs crates/khive-pack-knowledge/src/handlers.rs crates/khive-pack-knowledge/tests/integration.rs`: inspected the pushed fixes.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml -p khive-pack-knowledge topic -- --nocapture`: passed, 6 topic tests.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml -p khive-db test_query_by_tags -- --nocapture`: passed, 1 tag-query test.
- `RUSTC_WRAPPER= cargo fmt --manifest-path crates/Cargo.toml --all -- --check`: passed.

## What I Did Not Check

- I did not run full workspace tests or clippy.
- I did not fetch GitHub PR metadata; this review used the local branch and requested commit.

## Re-Review Guidance

No broad re-review needed. If the low normalization caveat is changed, a narrow check of `knowledge.learn(domain="Attention")`, `get(full_id)`, and `knowledge.topic(domain="attention")` is enough.
