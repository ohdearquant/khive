## Summary

<!-- 1-3 bullets: what changed and why. Match the actual diff. -->

## Test plan

<!-- Concrete commands and outcomes. Tick boxes as they pass. -->

- [ ] `cargo test --workspace` (in `crates/`)
- [ ] `cargo clippy --workspace -- -D warnings`
- [ ] `cargo fmt --all -- --check`
- [ ] `deno fmt --check docs/` (if docs changed)

## ADR

<!-- Every change to a verb's request/response fields or documented behavior
     (including budgets and ordering) must name the governing ADR change:
     link its hunk in this diff or the separate amendment PR and affected clauses.
     Do not merge without that reference and acceptance of the amendment.
     If the contract is unchanged, state that explicitly with evidence.
     For changes unrelated to verb contracts or architecture, write "n/a".
     See CONTRIBUTING.md, "Verb Contract Changes". -->

## AI-assisted contribution checklist

<!-- Required if any part of this diff was AI-generated (code, tests, docs,
     or this PR body). Delete the section if fully human-authored. -->

- [ ] Every claim in this PR description matches the actual diff
- [ ] Any agent-authored comment / PR body starts with an attribution line
- [ ] `cargo test` output included for behavior-changing code
- [ ] Behavior touching a typed relation (edges, entity/note kinds, `supersedes`/`annotates`, namespace) cites its governing ADR line — or flags it as an **unspecified design decision**, not a code invention (CLAUDE.md §"Data vs. view")

## Out of scope

<!-- What this PR intentionally does NOT do. -->
