# Implementation notes

## What changed

- Added the optional boolean `self_send` parameter to the `comm.send` `HandlerDef`, including
  its `false` default, configured-actor equality condition, and anonymous `local` exemption.
- Added a regression test that pins the `comm.send(help=true)` metadata contract for
  `self_send`.
- Updated the API reference, communication guide, and comm skill with the opt-in and identity
  collapse recovery guidance.
- Amended ADR-057 with the intentional compatibility decision and migration path for configured
  callers that send to their own actor label.

## Files

- `crates/khive-pack-comm/src/vocab.rs`
- `crates/khive-pack-comm/src/pack.rs`
- `docs/guide/api-reference.md`
- `docs/guide/communication.md`
- `marketplace/khive/skills/comm/SKILL.md`
- `docs/adr/ADR-057-comm-actor-addressed-delivery.md`

## Verification

- `cargo test -p khive-pack-comm send_declares_optional_self_send_contract --lib` — passed
  after first demonstrating the expected failure before the metadata fix.
- `cargo check -p khive-pack-comm` — passed.
- `cargo test -p khive-pack-comm` — passed: 211 tests plus doc-tests, 0 failures.
- `./scripts/ci.sh lint` — passed: rustfmt, SQL lint, ADR reference lint, and lint self-test.
- `./scripts/ci.sh clippy` — passed for the workspace with all targets/features and warnings
  denied.
- `git diff --check` — passed.
- `./scripts/ci.sh tests` — started and produced no observed failures, but was interrupted during
  long-running unrelated workspace tests to honor the fixed task deadline; the focused comm suite
  above completed in full.

## Domain utility

`domain_utility: {value: medium, note: "The schema-evolution briefing reinforced pinning the served help contract and documenting the compatibility migration; repository ADRs and existing pack tests supplied the concrete implementation pattern."}`
