Verdict: REJECT
Findings: 1 Blocker, 3 High, 1 Medium, 0 Low

## Findings

### [Blocker] `cargo test --workspace` fails on a stale bare JSON op

Evidence: `crates/khive-request/src/lib.rs:1007` still parses JSON-form tools named `"next"` and `"complete"`, while `crates/khive-request/src/lib.rs:1009` now expects `"gtd.complete"`. The workspace gate fails with:

```text
tests::json_form_with_leading_whitespace_inside_array_parses ... FAILED
assertion `left == right` failed
  left: "complete"
 right: "gtd.complete"
```

Why this matters: PR #471 is a wire-surface migration. A workspace test failure on exactly the old bare GTD verb shape means the branch does not meet the required quality gate, and the request parser test corpus still preserves pre-migration examples.

Suggested fix: Update the JSON fixture at `crates/khive-request/src/lib.rs:1007` to `gtd.next` / `gtd.complete`, then sweep the remaining parser test fixtures in the same file such as `assign(...)` / `next()` at `crates/khive-request/src/lib.rs:921`, `crates/khive-request/src/lib.rs:1022`, and `crates/khive-request/src/lib.rs:1194`.

### [High] Python contract tests still execute the old bare pack verbs

Evidence: `tests/khive-contract/tests/test_manifest.py:25` defines all product verbs with bare GTD and memory names at `tests/khive-contract/tests/test_manifest.py:30` and `tests/khive-contract/tests/test_manifest.py:32`. The executable smoke tests still call `khive_gtd_session.verb("assign", ...)` at `tests/khive-contract/tests/test_smoke.py:291` and `khive_memory_session.verb("remember", ...)` at `tests/khive-contract/tests/test_smoke.py:377`. Chain coverage still submits `assign(...) | complete(...)` at `tests/khive-contract/tests/test_chain_mode.py:69`. ADR-027 pack-selection coverage still uses JSON `{"tool": "assign"}` at `tests/khive-contract/tests/test_adr_027_single_tool_mcp.py:139`.

Why this matters: The task explicitly required Python contract tests under `tests/khive-contract/` to be updated. They were not in the PR diff and still encode the rejected surface, so downstream contract CI will either fail once run or continue certifying the wrong API.

Suggested fix: Namespace every non-kg product verb in the Python contract suite: `gtd.*`, `memory.*`, `comm.*`, `schedule.*`, and `knowledge.*`. Add a contract assertion mirroring `verb_namespace_contract.rs` so future Python tests cannot reintroduce bare non-kg verbs.

### [High] Public MCP/help text still teaches invalid bare pack calls

Evidence: `crates/khive-mcp/src/tools/request.rs:12` introduces request examples that still show `next()` at `crates/khive-mcp/src/tools/request.rs:13`, `assign(...)` at `crates/khive-mcp/src/tools/request.rs:14`, and JSON `{"tool":"next"}` / `{"tool":"complete"}` at `crates/khive-mcp/src/tools/request.rs:17`. `crates/khive-pack-schedule/src/lib.rs:60` exposes a `schedule.schedule` parameter description whose example payload is `remind(content=\"hello\")` instead of `schedule.remind(...)`.

Why this matters: These strings are not internal-only comments; they feed the request tool schema and handler help. Agents following the help text will emit bare pack verbs that ADR-023 §4 now rejects.

Suggested fix: Update all request/help examples to the dotted surface, including JSON-form examples. For schedule payload examples, use `schedule.remind(...)`, `gtd.transition(...)`, and `comm.send(...)` so deferred dispatch examples are valid under the same namespace rule.

### [High] Accepted ADRs still publish the pre-migration pack verb contract

Evidence: `docs/adr/ADR-019-gtd-pack.md:132` begins the GTD verb table with bare `assign`, `next`, `complete`, `tasks`, and `transition` through `docs/adr/ADR-019-gtd-pack.md:138`; `docs/adr/ADR-019-gtd-pack.md:222` still gives `assign(title=...)` as the GTD-native call. `docs/adr/ADR-021-memory-pack.md:101` and `docs/adr/ADR-021-memory-pack.md:133` still specify `remember(...)` and `recall(...)` signatures. `docs/adr/ADR-040-communication-and-schedule-packs.md:99` through `docs/adr/ADR-040-communication-and-schedule-packs.md:102` and `docs/adr/ADR-040-communication-and-schedule-packs.md:230` through `docs/adr/ADR-040-communication-and-schedule-packs.md:233` still list bare comm and schedule verbs, and cross-pack examples at `docs/adr/ADR-040-communication-and-schedule-packs.md:307` and `docs/adr/ADR-040-communication-and-schedule-packs.md:312` still schedule bare `transition` / `send` payloads.

Why this matters: ADR-023 §4 is now the accepted namespace contract, but other accepted ADRs remain contradictory in normative tables and examples. That leaves future implementation and review work with two conflicting accepted contracts.

Suggested fix: Update the accepted ADR corpus in the same PR, not only implementation snippets. Tables, signatures, examples, and "agents call" prose should use `gtd.assign`, `memory.recall`, `comm.send`, `schedule.schedule`, `knowledge.topic`, etc.; historical alternatives can keep old names only when explicitly marked as historical.

### [Medium] Top-level developer and agent docs still advertise the old surface

Evidence: `README.md:62` says loading GTD gives five more verbs and lists bare `assign`, `next`, `complete`, `tasks`, `transition` at `README.md:62` through `README.md:63`. `CLAUDE.md:189` through `CLAUDE.md:202` lists bare GTD and memory pack verbs in the MCP surface section. `AGENTS.md:35` through `AGENTS.md:43` still presents `remember` / `recall` as core bare verbs, and `AGENTS.md:20` links to the old `ADR-023-verb-consolidated-mcp-surface.md` path rather than the current ADR-023 file.

Why this matters: These are the first files developers and agents read. Even if the Rust registry is fixed, these docs will cause agents to generate calls that dispatch as unknown verbs after this migration.

Suggested fix: Update top-level documentation and agent guidance to reflect `kg` bare verbs only, with all optional pack verbs dotted. Also update stale ADR links to `docs/adr/ADR-023-declarative-pack-format.md` or whatever canonical ADR-023 path is intended.

## Looks Right

- Handler registrations and dispatch arms in the five renamed pack crates use the new dotted names: `memory.*`, `gtd.*`, `comm.*`, `schedule.*`, and `knowledge.*`.
- Exact sweeps for `name: "old_verb"` and `"old_verb" =>` in the renamed pack `src/lib.rs` files found no missed old handler names or dispatch match arms.
- `tests/smoke_test.py` uses dotted calls such as `gtd.assign`, `gtd.next`, `memory.remember`, and `memory.recall`.
- Marketplace executable `request(ops="...")` examples inspected by grep use dotted names. Remaining bare words there are mostly prose/status terms rather than dispatch examples.
- ADR-023 §4 contains the enforcement note and points to `crates/kkernel/tests/verb_namespace_contract.rs`.
- `crates/kkernel/tests/verb_namespace_contract.rs` force-links the built-in packs, rejects non-allowlisted bare names, rejects prefix mismatches, and rejects names with more than one dot. The positive contract test run passed.

## Commands Run

- `date -Iseconds`: `2026-05-25T22:49:03-04:00`.
- `git status --short --branch`: clean branch `v025-w4-verb-migration...origin/v025-w4-verb-migration`.
- `git diff --name-status 8837f23...HEAD`: inspected changed file set for PR #471.
- `rg -n 'name:\s*"...old verbs..."' crates/khive-pack-*`: no stale old `HandlerDef.name` hits in renamed packs.
- `rg -n '"...old verbs..."\s*=>' crates/khive-pack-*`: no stale dispatch-arm hits in renamed pack `lib.rs` files; one `"read"` hit was message status handling, not verb dispatch.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml -p kkernel --test verb_namespace_contract`: passed, 2 tests.
- `cargo fmt --manifest-path crates/Cargo.toml --all -- --check`: passed.
- `RUSTC_WRAPPER= cargo test --manifest-path crates/Cargo.toml --workspace`: failed in `khive-request` as described above.
- `RUSTC_WRAPPER= cargo clippy --manifest-path crates/Cargo.toml --workspace --all-targets -- -D warnings`: passed.
- `rg` stale-reference sweeps across `crates`, `tests`, `marketplace`, `docs`, `README.md`, `AGENTS.md`, and `CLAUDE.md`: found the stale references cited above.

## What I Did Not Check

- I did not run the Python contract suite because static inspection found executable stale bare verbs there and the Rust workspace test gate already failed.
- I did not inject a synthetic bad pack to force the namespace contract test negative path. I inspected the test logic and ran the registered-pack positive contract test.
- I did not inspect archived ADRs under `docs/_archive/adr_v0/`; old names there can remain historical.
- I did not post this review to GitHub.
- Lore domain utility: SKIPPED — no `mcp__lore__suggest` / `compose` tools are available in this environment, so I used the local khive review skill and accepted ADR corpus.

## Re-Review Guidance

Run a broad re-review after fixes, not a narrow one. The implementation registry looks mostly correct, but the stale surface spans Rust tests, Python contract tests, MCP help text, accepted ADRs, and top-level docs. Minimum re-check: rerun the stale bare-verb grep excluding `docs/_archive`, `cargo test --manifest-path crates/Cargo.toml --workspace`, `cargo clippy --manifest-path crates/Cargo.toml --workspace --all-targets -- -D warnings`, `cargo fmt --manifest-path crates/Cargo.toml --all -- --check`, and the relevant `tests/khive-contract` pytest suite.
