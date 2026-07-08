# IMPL_REPORT — khive-changeset (ADR-101 D1/D2/D5 crate #1)

## What was built

One new crate, `crates/khive-changeset`, added as a workspace member in `crates/Cargo.toml`
(inserted right after `khive-types`, matching that crate's position in the dependency chain).
Scope: exactly D1 (op-list model), D2 (NDJSON-delta serialization), and the crate-#1 slice of
D5 (no filesystem/IO, wasm32-compilable, wasm-parity CI) from ADR-101. No rule evaluator, no
diff computation, no ingester, no CLI wiring — confirmed none of that surface was touched.

### Files

- `crates/khive-changeset/Cargo.toml` — depends only on `khive-types` (`features = ["serde"]`),
  `serde`, `serde_json` (`features = ["float_roundtrip"]`), `thiserror`; dev-dependency
  `proptest` pinned directly (not `workspace = true`) with `default-features = false,
  features = ["std"]`.
- `src/lib.rs` — pure re-export shim, 1-line doc comment, no ADR citations (per publication
  hygiene).
- `src/envelope.rs` — `Envelope` (`schema_version`, `producer`, `producer_model_family`,
  `staged_at`), `CURRENT_SCHEMA_VERSION = 1`.
- `src/op.rs` — `Op` enum (`Create`/`Link`/`Update`/`Delete`/`Merge`) and the five ops'
  payload types, built directly on `khive_types::{EntityKind, EdgeRelation, Entity, Note,
  Link, PropertyValue, Id128, Namespace}` — no kind/relation vocabulary redefined.
- `src/changeset.rs` — `ChangeSet` struct, `ChangeSetError` (via `thiserror`), `to_ndjson` /
  `from_ndjson`.
- `tests/roundtrip.rs` — `proptest`-based round-trip property coverage for all 5 op kinds
  (including the two preimage-bearing ones), envelope round-trip, and op-order preservation
  under arbitrary id sequences.
- `README.md` — crate-level docs, including the ADR-101/ADR-102 citations (kept out of `.rs`
  files per the publication-hygiene rule) and the "Known gap" section below.
- `.github/workflows/ci.yml` — new `wasm-parity` job, added to `ci-gate`'s `needs` list.

## Design decisions (judgment calls)

1. **Envelope as a header line, not a sidecar** (D2 explicitly leaves this open). Chose header
   line: a change-set is meant to move as one artifact between producer, reviewer, and
   applier; a sidecar file can go missing or drift out of sync with its NDJSON body. Line 1 =
   envelope; lines 2..N = ops in stage order.
2. **Non-flattened nesting everywhere** (`CreateOp.target: CreateTarget`,
   `UpdateOp.patch: UpdatePatch`) instead of `#[serde(flatten)]`. Avoids a real collision I hit
   during implementation: `khive_types::Entity` and `khive_types::Note` each already own a
   `kind` field (entity kind / note kind respectively). `DeletePreimage`'s own internally-tagged
   discriminant is `substrate` (not `kind`) for the same reason — tagging it `kind` produced a
   genuine `serde_json` "duplicate field `kind`" parse error against `Entity`/`Note` preimages.
   Caught by the round-trip proptest suite, not by inspection.
3. **`serde_json/float_roundtrip` is load-bearing, not optional.** Without it, the default
   float parser is a fast approximation that does not always recover the exact f64 bits a
   prior serialize wrote — `weight`/`salience`/`decay_factor` values drifted by an ULP on
   re-serialization in the proptest suite (`0.16978499356576301` → `0.169784993565763`) before
   the feature was enabled. This is a real, previously-undocumented serde_json gotcha for any
   crate promising byte-identical round-trips.
4. **`LinkOp` validates weight range at deserialize time** via a `TryFrom`/`#[serde(into =
   ...)]` shim, mirroring `khive_types::Link::is_valid`'s exact pattern (`[0.0, 1.0]`,
   finite). Kept for consistency with the type this op eventually produces.
5. **Update-patch tri-state fields** (`description`, `salience`, `decay_factor`) use
   `Option<Option<T>>` via a small `opt_opt` serde helper module, mirroring the same pattern
   already present in `khive-types::event::ProposalEntityPatch`'s `serde_opt_opt` — reused the
   established codebase idiom rather than inventing a new one.
6. **`deny_unknown_fields` on every wire-level struct**, matching the `kkernel::kg::validate`
   precedent cited in the task spec. Verified by dedicated unit tests (envelope, `CreateOp`)
   and the malformed-line integration tests.
7. **`schema_version` on the envelope; unrecognized version is a hard error**, not a
   best-effort parse — `from_ndjson` rejects any `schema_version != CURRENT_SCHEMA_VERSION`
   before touching the op lines. Tested explicitly.
8. **wasm-parity CI actually executes the test suite** rather than compile-checking twice.
   `wasm32-unknown-unknown` has no standalone test-execution story (no stdout without extra JS
   tooling this repo does not otherwise depend on), so the CI job compile-checks
   `wasm32-unknown-unknown` (the literal D5 constraint) and separately *runs* the identical
   test suite under `wasm32-wasip1` via a pinned `wasmtime` runner, then diffs the sorted
   `test <name> ... ok/FAILED` lines between the native and wasm32 runs — failing the job on
   any divergence. Verified working locally (see "wasm-parity evidence" below) before writing
   the CI job, not written speculatively.
9. **`proptest` dev-dependency pinned directly** (not `workspace = true`) with
   `default-features = false, features = ["std"]`. Discovered locally: proptest's default
   `fork`/`timeout` features pull in `rusty-fork` → `wait-timeout`, which needs process
   fork/wait and does not compile for `wasm32-wasip1`. Also discovered a real Cargo wrinkle:
   overriding `default-features = false` against a workspace-inherited dependency
   (`workspace.dependencies.proptest = "1"`, no explicit `default-features` there) is silently
   ignored by Cargo with a warning — had to depend on a pinned version directly instead of via
   `workspace = true` to make the override take effect.

## ADR ambiguity encountered (not resolved by inventing schema)

**`UpdateOp` carries no preimage — this is a genuine tension between ADR-101 D1 and ADR-102 D4,
not something I resolved by guessing.**

- ADR-101 D1 (the artifact I implement) is explicit and scoped: *"Destructive operations
  capture their preimage at stage time. A `delete` op records the full prior state... and a
  `merge` op records both prior entities and the incident edges..."* — preimage capture is
  required only for `delete` and `merge`, the two operations D1 itself names "destructive."
  `update` is not in that set.
- ADR-102 D4 (a different, downstream ADR I do not implement, but whose stated needs I was
  asked to keep in mind) describes revert semantics as if every op has one:
  *"an `update`'s inverse restores the prior field values captured at stage time"* — this
  presumes `update` ops capture prior values, which D1 never requires.

I followed ADR-101 D1's literal, binding text (the ADR I am implementing) rather than
speculatively adding an optional prior-value field to satisfy a different ADR's assumption.
`UpdateOp` therefore has no preimage in this crate. This is flagged in `README.md` under
"Known gap" for whoever picks up ADR-102's implementation or amends ADR-101. The two most
likely resolutions — (a) accept the gap and always fall back to the git-revert backstop for
`update` reverts, or (b) amend ADR-101 to add optional stage-time prior-value capture to
`update` — are both real ADR-author decisions, not implementation-crate decisions, so I did
not pick one.

## Gate results (real exit codes, run from `crates/`, `CARGO_TARGET_DIR` inside the worktree)

| Gate | Command | RC |
|---|---|---|
| fmt (crate) | `cargo fmt -p khive-changeset -- --check` | 0 |
| fmt (all) | `cargo fmt --all -- --check` | 0 |
| clippy (crate) | `cargo clippy -p khive-changeset --all-targets -- -D warnings` | 0 |
| clippy (workspace) | `cargo clippy --workspace --all-targets -- -D warnings` | 0 |
| test | `cargo test -p khive-changeset` | 0 (25 tests: 14 unit + 11 proptest integration) |
| check (workspace) | `cargo check --workspace` | 0 |
| check (wasm32-unknown-unknown) | `cargo check -p khive-changeset --target wasm32-unknown-unknown` | 0 |
| doc build | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p khive-changeset` | 0 |

All gates re-run and confirmed green after the final edit pass (removing ADR citations from
`.rs` files per publication hygiene).

## wasm-parity evidence

Verified **locally**, end to end, before writing the CI job (installed `wasmtime` 46.0.1 via
Homebrew, added the `wasm32-wasip1` rustup target):

```
$ export CARGO_TARGET_WASM32_WASIP1_RUNNER="wasmtime run --wasi preview2 --"
$ cargo test -p khive-changeset --target wasm32-wasip1
running 14 tests   (src/lib.rs unit tests)     ... 14 passed; 0 failed
running 11 tests   (tests/roundtrip.rs)        ... 11 passed; 0 failed
```

Then ran the exact parity-check script the new CI job uses (`grep` both native and wasm32 logs
for `^test .* \.\.\. (ok|FAILED)$` lines, sort, `diff -u`):

```
$ diff -u /tmp/native.sorted /tmp/wasm.sorted && echo PARITY_OK
PARITY_OK
```

25/25 test names identical between native and `wasm32-wasip1`, all `ok` on both sides, byte
for byte on the sorted result lines. This confirms the crate is not merely
`wasm32-unknown-unknown`-compilable (also verified separately, see gate table) but that its
full behavior — including the `serde_json::float_roundtrip`-dependent byte-identical
round-trip property — holds identically under real wasm execution, not just native.

The new `wasm-parity` CI job (`.github/workflows/ci.yml`) reproduces this exact sequence:
`rustup` targets `wasm32-unknown-unknown,wasm32-wasip1` via `dtolnay/rust-toolchain`, a pinned
`wasmtime` 46.0.1 install, a `wasm32-unknown-unknown` compile check, then native and
`wasm32-wasip1` test runs diffed for parity. Added to `ci-gate`'s `needs` list so it is a
required check.

## Commit(s)

- `feat(changeset): op-list change-set model and NDJSON-delta codec (ADR-101 D1/D2/D5)`
  — not pushed, per instructions.

## Not done (explicitly out of scope, confirmed untouched)

- Rule evaluator (ADR-101 D5 crate #2).
- Diff computation (ADR-101 D5 crate #3).
- Any ingester (ADR-101 D3).
- Any CLI wiring (`kkernel kg commit` etc., ADR-102).
- No producer-specific shape leaked into the op-list — `Op`, `CreateOp`, `LinkOp`, `UpdateOp`,
  `DeleteOp`, `MergeOp` name no producer and carry only kind/substrate/fields/identifiers/
  preimage, per D1.

## Fix round (codex APPROVE-WITH-FIXES, on top of `4fea8a80`)

Codex review returned APPROVE-WITH-FIXES (0 Blocker, 2 High, 1 Medium) and committed three
failing probe tests into `tests/roundtrip.rs` as the oracle for "done." All three now pass
unmodified.

1. **High-1 — `EdgePatch.weight` had no validation.** Rewrote `EdgePatch` with the same
   raw-shim pattern `LinkOp` already used (`#[serde(into = "EdgePatchRaw")]` + manual
   `Deserialize` that round-trips through `TryFrom<EdgePatchRaw>`), enforcing finite +
   `[0.0, 1.0]` at deserialize time. Fixes `probe_rejects_out_of_range_edge_patch_weight`.
2. **High-2 — destructive preimages weren't checked against their op's target IDs.** Gave
   `DeleteOp` and `MergeOp` custom `Deserialize` impls (via `DeleteOpRaw`/`MergeOpRaw` shadow
   structs) that compare `target_id` against `preimage.record_id()` (an internal
   `DeletePreimage` helper matching on the `Entity`/`Note`/`Edge` variant), and `into_id`/
   `from_id` against `preimage.into.header.id`/`preimage.from.header.id`. Also added the
   suggested (not strictly probed) check that every `MergePreimage.incident_edges` entry
   references at least one of `into_id`/`from_id` — `khive_types::Link` exposes `source`/
   `target` as plain `pub Id128` fields, so this was a direct, non-awkward addition, not
   skipped. Fixes `probe_rejects_delete_preimage_with_mismatched_record_id`.
3. **Medium-3 — full-record preimages silently dropped unknown fields.** `DeletePreimage`
   and `MergePreimage` embed `khive_types::{Entity, Note, Link}` directly, and those types'
   own `Deserialize` impls do not `deny_unknown_fields` (out of scope to change — `khive-types`
   was not touched). Added a new `src/strict.rs` module (`pub(crate)`, not re-exported) with
   `StrictEntity`/`StrictNote`/`StrictLink` mirror structs carrying
   `#[serde(deny_unknown_fields)]`, converted into the real types via `From`/`TryFrom` impls
   that reuse each type's own `is_valid()` rather than re-deriving range checks. Applied to
   **both** `DeletePreimage` (explicitly probed) and `MergePreimage` (not probed, but the same
   bug class applies identically to `MergePreimage.into`/`.from`/`.incident_edges` — fixing
   only the probed path would have left an inconsistent, likely-flagged-on-re-review gap).
   Fixes `probe_rejects_unknown_field_inside_delete_preimage`.

New file: `src/strict.rs` (3 unit tests: `strict_entity_rejects_unknown_field`,
`strict_link_rejects_out_of_range_weight`, `strict_note_rejects_out_of_range_salience`).
`lib.rs` gained `mod strict;` (crate-private — no new public API surface).

### Fix-round gate results (real exit codes, from `crates/`, worktree-local `CARGO_TARGET_DIR`)

| Gate | Command | RC |
|---|---|---|
| fmt (all) | `cargo fmt --all -- --check` | 0 (first run was 1 — new match-arm formatting; fixed via `cargo fmt --all`, re-checked clean) |
| clippy (crate) | `cargo clippy -p khive-changeset --all-targets -- -D warnings` | 0 |
| clippy (workspace) | `cargo clippy --workspace --all-targets -- -D warnings` | 0 |
| test (native) | `cargo test -p khive-changeset` | 0 (31 tests: 17 unit + 14 integration, incl. all 3 probes) |
| check (workspace) | `cargo check --workspace` | 0 |
| check (wasm32-unknown-unknown) | `cargo check -p khive-changeset --target wasm32-unknown-unknown` | 0 |
| doc build | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p khive-changeset` | 0 |
| test (wasm32-wasip1, wasmtime 46.0.1) | `CARGO_TARGET_WASM32_WASIP1_RUNNER="wasmtime run --wasi preview2 --" cargo test -p khive-changeset --target wasm32-wasip1` | 0 (31/31 tests, identical names/outcomes to native — full parity, incl. all 3 probes) |

All three probes (`probe_rejects_out_of_range_edge_patch_weight`,
`probe_rejects_delete_preimage_with_mismatched_record_id`,
`probe_rejects_unknown_field_inside_delete_preimage`) pass unmodified, natively and under
`wasm32-wasip1`/wasmtime.

### Commit

- `fix(changeset): validate edge weight, preimage-id consistency, and preimage unknown fields`
  — additive, on top of `4fea8a80`, not pushed, per instructions.
