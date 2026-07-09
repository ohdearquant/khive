# S1 Implementation Report — `resolve_reference` + recently-referenced ring

**Branch**: `feat/resolve-s1` (worktree `/Users/lion/khive-work/worktrees/khive-resolve-s1`)
**Commit**: `b1d2a3b4` — "feat(runtime): resolve_reference capability + recently-referenced ring (S1)"
**Spec**: `.khive/workspaces/20260709/unified-verb/DRAFT-ADR-unified-verb.md`, S1 row of the
staged-delivery slice table.

## Scope delivered (exactly S1, nothing from S2-S4)

1. Runtime capability `resolve_reference(runtime, ring, token, nl_ref, limit, entity_kind)
   -> Resolved{id, confidence} | Ambiguous{candidates} | NotFound`.
2. Daemon-warm recently-referenced ring, admitted at the dispatch boundary under the strict
   by-id-only rule.
3. Thin read-only `resolve(refs, kind?, limit?)` verb in the kg pack.

No `ask` verb, no planner, no `Rewriter` trait, no `ConsumerKind::Ask` — confirmed out of
scope per the slice table and not touched.

## Files touched

- `crates/khive-runtime/src/reference_ring.rs` (new) — `ReferenceRing`, `RingEntry`,
  `ring_admissions_for()`. 11 unit tests.
- `crates/khive-runtime/src/reference_resolution.rs` (new) — `resolve_reference()`,
  `ReferenceResolution`, `ReferenceCandidate`. 6 unit tests.
- `crates/khive-runtime/src/pack.rs` — `VerbRegistry.reference_ring: Arc<ReferenceRing>`
  field + `reference_ring()` accessor; admission block inserted into
  `dispatch_with_identity` right after the existing post-dispatch hook, before `return
  result;` (~pack.rs:1279-1330, search for "Recently-referenced ring admission").
- `crates/khive-runtime/src/lib.rs` — module declarations + re-exports
  (`resolve_reference`, `ReferenceCandidate`, `ReferenceResolution`, `ReferenceRing`,
  `RingEntry`).
- `crates/khive-pack-kg/src/handlers/resolve.rs` (new) — `handle_resolve`, thin wrapper.
- `crates/khive-pack-kg/src/handlers/params.rs` — `ResolveParams`.
- `crates/khive-pack-kg/src/handlers/mod.rs` — registers the `resolve` module.
- `crates/khive-pack-kg/src/handler_defs.rs` — `KG_HANDLERS` bumped 17 → 18, new
  `HandlerDef` for `resolve`.
- `crates/khive-pack-kg/src/dispatch.rs` — routes `"resolve"` to `handle_resolve`; 7 new
  integration tests appended to the existing `#[cfg(test)] mod tests` block.
- `crates/khive-pack-kg/src/handlers/tests.rs` — renamed/updated the pinned handler-count
  test (17 → 18).
- `crates/khive-pack-kg/tests/integration.rs` — renamed/updated the pinned verb-count test
  (17 → 18) and the verb-names list.
- `tests/smoke_test.py` — updated the pinned total-verb-surface assertion (76 → 77).

## Where the ring lives, and how it's wired

`ReferenceRing` (`crates/khive-runtime/src/reference_ring.rs`) is a `Mutex<HashMap<(String,
String), VecDeque<RingEntry>>>` keyed by `(namespace, actor)`, held as `Arc<ReferenceRing>`
inside `VerbRegistry` (`pack.rs:~745`, field `reference_ring`). `VerbRegistry` derives
`Clone` and is Arc-wrapped throughout, so every clone of a warm registry (the ADR-049
daemon-warm object) shares the *same* ring instance — this is what makes it survive across
requests within a daemon's lifetime and empty on restart, matching the spec's "daemon-warm
memory... never persisted" requirement.

Admission is wired at the single dispatch chokepoint, `VerbRegistry::dispatch_with_identity`
(`pack.rs`, inside the per-pack dispatch loop, right after the existing opt-in
`DispatchHook` block and before `return result;`):

```rust
if let Ok(ref ok_val) = result {
    let admissions = crate::reference_ring::ring_admissions_for(verb, ok_val);
    if !admissions.is_empty() {
        let actor_key = format!("{}:{}", gate_req.actor.kind, gate_req.actor.id);
        for (id, name) in admissions {
            self.reference_ring.admit(ns.as_str(), &actor_key, id, name);
        }
    }
}
```

This runs unconditionally (not gated on the optional `DispatchHook`, since the ring is a
core capability, not an observer) for every successful dispatch through the registry.
`ring_admissions_for(verb, result)` inspects only the already-serialized JSON result — no
extra storage reads — and admits:

- `create` / `get` / `update` / `delete`: the `id` field plus a best-effort `name` (prefers
  `name`, falls back to a 60-char `content` snippet, else `None`).
- `merge`: `kept_id` only (not `removed_id`).
- `link`: both `source_id` and `target_id` (singleton form only).
- Bulk shapes (`items=[...]`, `links=[...]`) are excluded — identified by an `attempted`
  count in the response, and out of S1 scope per the "caller named or received one specific
  id" semantic (plural ids don't fit that contract cleanly; this was a deliberate scope call,
  not an oversight — noted in the module doc comment).
- `search` / `list` never reach the admission function at all (not in the verb match).

## Resolution order (Layer 0)

`resolve_reference` in `reference_resolution.rs`:

1. **Id-string passthrough.** `Uuid::from_str` or an 8+ hex-char prefix routes through the
   *existing* `KhiveRuntime::resolve_by_id` / `resolve_prefix_unfiltered` — not
   reimplemented. A miss here is `NotFound`, not a fall-through to search (the caller named a
   specific id). Verified this repo's only "slug" concept is knowledge-pack-atom-specific
   (grep confirmed zero `slug` usage in kg/entity/note code) — the "slug" mention in the S1
   scope line refers to that pack's own resolution, not something kg-level `resolve` needed
   to replicate.
2. **Ring match.** Exact case-insensitive name match (confidence 0.95) first, then substring
   match either direction (confidence 0.7). A single match at/above 0.7 resolves; multiple
   exact or multiple substring matches are `Ambiguous`.
3. **Hybrid-search fallback.** `KhiveRuntime::hybrid_search` (existing entity search, RRF
   fusion). Since RRF scores aren't on a fixed 0-1 confidence scale, auto-resolve uses a
   decisive-margin rule (top candidate ≥ 2x the runner-up) rather than a fixed absolute bar;
   otherwise every hit above a zero floor is returned as an `Ambiguous` candidate set.

`Ambiguous` always lists what was found; nothing is ever silently picked (F7 of the ADR).

## Gates — real exit codes, scoped per the task brief

Run from `/Users/lion/khive-work/worktrees/khive-resolve-s1/crates` with
`CARGO_TARGET_DIR=/Users/lion/khive-work/worktrees/khive-resolve-s1/target`:

| Gate | Command | rc |
|---|---|---|
| fmt | `cargo fmt --all -- --check` | 0 |
| clippy | `cargo clippy -p khive-runtime -p khive-pack-kg --all-targets -- -D warnings` | 0 |
| tests | `cargo test -p khive-runtime -p khive-pack-kg` | 0 (848 tests passed, 0 failed, 5 ignored doctests) |
| workspace check | `cargo check --workspace` | 0 |

Full-workspace `cargo test` was intentionally NOT run (out of scope per the task brief — the
4246-test suite times out gate agents).

## Tests added (17 new)

`khive-runtime::reference_ring::tests` (11): admission + name extraction, ring-admissions-for
per verb (get/link/merge/bulk/search-list-empty), size eviction, age/TTL eviction, actor
isolation, namespace isolation, re-admission moves to most-recent without duplicating.

`khive-runtime::reference_resolution::tests` (6): id-string passthrough resolves, id-string
passthrough never errors on a miss (returns `NotFound`), ring exact match resolves without
search, ring ambiguous on duplicate exact matches, `NotFound` when nothing matches at any
stage, actor isolation blocks cross-actor ring reads.

`khive-pack-kg::dispatch::tests` (7, end-to-end through `registry.dispatch`, which is the
path that actually exercises the admission hook — most of this file's existing tests call
`pack.dispatch(...)` directly, which bypasses `dispatch_with_identity` and therefore never
admits to the ring; this distinction is called out in a comment at the top of the new test
block since it's an easy trap to fall into): id-string passthrough via `resolve`, ring
resolution after `create` (asserts the exact 0.95 confidence), ambiguous on duplicate ring
names, not-found on an empty graph, search-never-populates-the-ring (entity created directly
on the runtime bypassing dispatch, then only `search`ed — proven by confidence < 0.9, below
both ring bands), and `resolve` appearing in `verbs()` introspection.

## One gotcha worth persisting

Most of `khive-pack-kg/src/dispatch.rs`'s existing test suite calls `pack.dispatch(verb,
params, &registry, &tok)` directly — this bypasses `VerbRegistry::dispatch_with_identity`
entirely (the gate check, audit events, and the new ring-admission hook all live there, not
in `PackRuntime::dispatch`). Any test that needs to observe ring admission (or gate/audit
behavior) must go through `registry.dispatch(verb, params)` instead, using the registry's own
baked identity rather than an explicitly-constructed token. First draft of these tests used
`pack.dispatch(...)` and silently exercised nothing — they passed for the wrong reason
(empty-ring fallback to hybrid search happened to still resolve the single-entity case). Fixed
before commit; flagging so the next S2/S3 implementer doesn't repeat it.

## Latency contract note

The S1 latency contract (Tier 1, p50 ≤15ms / p99 ≤20ms) was not independently benchmarked in
this pass — the admission path adds one `Mutex` lock + a `HashMap` entry lookup + a
bounded `VecDeque` scan (≤64 entries) per dispatch, and `resolve_reference`'s id-string and
ring stages are in-memory only; the hybrid-search fallback (stage 3) is the existing
`hybrid_search` path with its own already-measured cost. No new I/O was introduced on the
hot dispatch path. A dedicated measurement pass against the scripted-session benchmark named
in the S1 gate/acceptance row is follow-up work, not part of this implementation task.
