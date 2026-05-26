Verdict: REQUEST CHANGES
Findings: 0 Critical, 2 High, 0 Medium, 0 Low

## Findings

### [High] DB-level domain filtering is no longer case-insensitive

Evidence: `docs/adr/ADR-047-knowledge-pack.md:91` requires the domain filter to be a case-insensitive tag match. `crates/khive-pack-knowledge/src/handlers.rs:206`-`210` only trims the requested domain, `crates/khive-runtime/src/operations.rs:466`-`468` passes that string unchanged into `EntityFilter::tags_any`, and `crates/khive-db/src/stores/entity.rs:231`-`242` implements the SQL as `json_each.value IN (...)` with no `LOWER(...)` or `COLLATE NOCASE`.

Why this matters: K-3 pushed the listing path filter into SQL, but changed the public contract. A concept learned with `domain="Attention"` will not be returned by `knowledge.topic(domain="attention")` on the no-query listing path, even though ADR-047 says it must match.

Suggested fix: make the SQL tag predicate case-insensitive, for example `LOWER(json_each.value) IN (LOWER(?), ...)`, or normalize both stored domain tags and query tags deliberately. Add a regression that learns a mixed-case domain and queries it with different case.

### [High] Search-path `total` is still capped by a bounded candidate pool

Evidence: `crates/khive-pack-knowledge/src/handlers.rs:215`-`218` calls `hybrid_search(..., limit * 4, ...)`, then `crates/khive-pack-knowledge/src/handlers.rs:250`-`253` computes `total = filtered.len()` before taking `limit`. The runtime contract at `crates/khive-runtime/src/retrieval.rs:168` says the supplied `limit` caps the returned list, and `crates/khive-runtime/src/retrieval.rs:260` truncates to that limit. The response then publishes that bounded count at `crates/khive-pack-knowledge/src/handlers.rs:271`.

Why this matters: K-6 claims `total` is the pre-limit match count on both paths. On the query path it is only the count inside the `limit * 4` returned search window, not the true pre-limit match count. If more than `limit * 4` concepts match, `total` under-reports; with a domain filter, matching concepts ranked outside the candidate window can be silently missed.

Suggested fix: either compute a real pre-limit total for `knowledge.topic(query=...)`, or stop claiming `total` is pre-limit for the search path and document it as the bounded candidate count. Add a regression with more than `limit * 4` query matches.

## Looks Right

- K-2 mostly has unified item fields on both topic paths: `id`, `full_id`, `name`, `description`, and `tags`; the search path adds `score` and optional `snippet` (`crates/khive-pack-knowledge/src/handlers.rs:256`-`266`, `crates/khive-pack-knowledge/src/handlers.rs:289`-`295`). Both paths use `short_id`, so `id` is 8 chars (`crates/khive-pack-knowledge/src/handlers.rs:28`-`30`).
- K-3 did move the no-query listing filter and count to the DB layer (`crates/khive-runtime/src/operations.rs:453`-`483`, `crates/khive-runtime/src/operations.rs:489`-`509`), and the SQL applies the tag filter before `LIMIT` (`crates/khive-db/src/stores/entity.rs:231`-`242`, `crates/khive-db/src/stores/entity.rs:435`-`439`).
- K-4 matches ADR-002 for concept `introduced_by` targets: document or person (`docs/adr/ADR-002-edge-ontology.md:187`-`188`, `crates/khive-pack-knowledge/src/lib.rs:83`-`87`).
- K-5 matches ADR-047: topic limit defaults to 20 and caps at 100 (`docs/adr/ADR-047-knowledge-pack.md:89`-`90`, `crates/khive-pack-knowledge/src/handlers.rs:204`-`205`, `crates/khive-pack-knowledge/src/lib.rs:115`-`119`).
- K-7 distinguishes unknown packs from missing dependencies at runtime and in MCP error display (`crates/khive-runtime/src/pack.rs:1132`-`1159`, `crates/khive-runtime/src/pack.rs:1202`-`1218`, `crates/khive-mcp/src/server.rs:117`-`128`, `crates/khive-mcp/src/server.rs:185`-`192`).
- No new entity kinds or edge relations were added by this diff.

## Commands Run

- `date -Iseconds`: confirmed review start within deadline.
- `git status --short --branch`: clean branch `w5-knowledge-topic...origin/w5-knowledge-topic`.
- `git diff --stat main..HEAD` and `git diff --name-only main..HEAD`: confirmed the six changed files in scope.
- `cargo test -p khive-pack-knowledge topic -- --nocapture`: failed because this CWD is not the Cargo workspace root.
- `cargo test --manifest-path crates/Cargo.toml -p khive-pack-knowledge topic -- --nocapture`: failed because sandbox blocked `sccache`.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml -p khive-pack-knowledge topic -- --nocapture`: passed, 4 topic tests.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml -p khive-mcp pack_gtd_without_kg_fails_at_boot -- --nocapture`: passed, 1 test.
- `RUSTC_WRAPPER= cargo fmt --manifest-path crates/Cargo.toml --all -- --check`: passed.

## What I Did Not Check

- I did not run full workspace tests, clippy, or coverage due to the 8-minute budget.
- I did not fetch GitHub PR metadata; the review used the requested local `main..HEAD` diff.
- Lore/domain tools were requested by the role guidance but no `mcp__lore__suggest` or `mcp__lore__compose` tools are available in this session.

## Re-Review Guidance

Re-review narrowly after fixes: focus on case-insensitive DB tag filtering, the query-path `total` semantics, and tests covering both regressions.

Domain utility: SKIPPED — lore composition tools are not available in this environment.
