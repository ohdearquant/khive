# Fix summary — PR #853 codex fix-round

Branch: `fix/452-rego-secret-leak`
Scope: `crates/khive-gate-rego/src/gate.rs`, `crates/khive-gate-rego/Cargo.toml`

## HIGH — deserialization diagnostics still leaked caller data to logs (GATEREGO-AUD-002)

`crates/khive-gate-rego/src/gate.rs:253-257` logged the `serde_json::from_str::<GateDecision>`
deserialization error with `error = %e`. `GateDecision` is an internally-tagged enum
(`#[serde(tag = "decision")]`), so when a malformed policy returns
`{"decision": input.args.changeset.entity.properties.api_key}`, serde's "unknown variant"
error text embeds the caller-supplied value verbatim (e.g. `` unknown variant
`AKIAFAKEKEY000000000`, expected `allow` or `deny` ``). The returned `Deny` reason was already
sanitized (GATEREGO-AUD-001), but this second channel, the `tracing::warn!` call on the same
wrong-shape path, was not.

Fix: dropped `error = %e` entirely from that log statement and replaced it with a fixed,
caller-data-free category string (`error = "policy_decision_shape_mismatch"`). The log now
carries only `entrypoint`, the type-shape summary (`shape`, already sanitized per
GATEREGO-AUD-001), and the fixed category, none derived from caller input.

Regression test added: `gate::tests::malformed_policy_shape_mismatch_log_does_not_leak_secret`.
It installs a scoped `tracing_subscriber::fmt` subscriber writing into an in-memory buffer
(`CapturedLog`, a small `MakeWriter` impl added as a test helper), drives `RegoGate::check`
against a policy that evaluates successfully to
`{"decision": input.args.changeset.entity.properties.api_key}` (so the wrong-shape
deserialize branch is actually exercised, not a Rego-eval failure), and asserts:
- the captured log is non-empty (the branch fired),
- it contains the fixed `policy_decision_shape_mismatch` category (proving the intended branch,
  not a different one, produced the log),
- it does **not** contain the fake secret value or the `api_key` field name.

`tracing-subscriber` (already a workspace dependency) was added as a dev-dependency of
`khive-gate-rego` to support this capture.

Note: the sibling pre-existing test (`malformed_policy_echoing_input_args_does_not_leak_secret`,
using `default decision := input.args`) does not actually reach the deserialize-shape-mismatch
branch — regorus rejects a `ref` inside a `default` rule body at eval time ("invalid `ref` in
default value"), so that policy fails during `eval_rule`, not during `serde_json::from_str`. It
still validates GATEREGO-AUD-001 (the eval-failure log/Deny-reason path never carried the raw
value to begin with), just not the specific deserialize path GATEREGO-AUD-002 targets. Left
that test untouched (out of scope) and added the new test with a policy shape confirmed via a
throwaway probe to actually reach the deserialize branch.

## LOW — em-dashes (publication hygiene)

Fixed the three em-dashes codex flagged, all in text this PR itself introduced:
- `gate.rs:241` → replaced `— was` with `(the raw policy output) was`.
- `gate.rs:272` (doc comment) → replaced `— used to` with `, used to`.
- `gate.rs:347` (test comment) → replaced the em-dash pair with parentheses.

Pre-existing em-dashes elsewhere in the file (lines 55, 100, 144, 179, 200, 226 — not touched by
this PR's diff) were left alone; they weren't flagged by codex and are out of this PR's scope.

## Verification (scoped, from `crates/`)

- `cargo fmt -p khive-gate-rego` — clean.
- `cargo clippy -p khive-gate-rego --all-targets -- -D warnings` — clean.
- `cargo test -p khive-gate-rego` — 25 passed (24 previously + 1 new regression test), 0 failed.
