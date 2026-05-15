## Summary

<!-- 1-3 bullets: what changed and why. Match the actual diff. -->

## Test plan

<!-- Concrete commands and outcomes. Tick boxes as they pass. -->

- [ ] `cargo test --workspace` (in `crates/`)
- [ ] `cargo clippy --workspace -- -D warnings`
- [ ] `cargo fmt --all -- --check`
- [ ] `deno fmt --check docs/` (if docs changed)

## ADR

<!-- If this PR introduces a significant design change, link to the ADR
     (docs/adr/ADR-NNN-*.md). For small fixes, write "n/a". -->

## AI-assisted contribution checklist

<!-- Required if any part of this diff was AI-generated (code, tests, docs,
     or this PR body). Delete the section if fully human-authored. -->

- [ ] Every claim in this PR description matches the actual diff
- [ ] Any agent-authored comment / PR body starts with an attribution line
- [ ] `cargo test` output included for behavior-changing code

## Out of scope

<!-- What this PR intentionally does NOT do. -->
