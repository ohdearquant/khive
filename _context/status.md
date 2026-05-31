# Runtime Namespace Isolation — Implementation Status

khive stats at task start: entities=2475, edges=7540, notes=11577

## Per-Issue Table

| Issue | Root Cause (file:line) | Fix Applied | Test Name + Result | Status |
|-------|----------------------|-------------|-------------------|--------|
| #569 | `operations.rs` and `curation.rs` had scattered inline `!=` namespace comparisons in resolve/delete_note/update_note with no shared helper | Added `pub(crate) fn ensure_namespace(record_ns, caller_ns) -> RuntimeResult<()>` at `operations.rs:404`; routed resolve-note (`ops.rs:1573`), delete_note (`ops.rs:1609`), update_note (`curation.rs:461`) through it | (covered by existing tests + isolation tests) | RESOLVED |
| #548 | (a) `get_edge()` at `operations.rs:1841` skipped namespace check entirely; (b) round-1 fix used `ensure_namespace(...)?` which returned `Err(NotFound)` for foreign IDs vs `Ok(None)` for absent — an existence oracle violating ADR-007:217 | Raw SQL probe before scoped fetch; foreign-namespace branch changed from `?`-propagate to `if is_err() { return Ok(None) }` at `operations.rs:1864` — both absent and foreign now return `Ok(None)` | `get_edge_cross_namespace_returns_none` (+ absent-vs-foreign equivalence assertion) — PASS | RESOLVED |
| #567 | (a) Runtime: `merge_note()` at `curation.rs:511` entered SQL transaction before namespace checks (fixed round-1); (b) Pack: `ensure_note_kind` at `handlers.rs:1120` called `runtime.notes(token)?.get_note(id)` — `NoteStore::get_note` is ID-only (no namespace filter), leaking foreign note existence and kind before runtime denial | Runtime: both notes fetched + `ensure_namespace` called at `curation.rs:545,551` before SQL. Pack: replaced `notes(token)?.get_note(id)` with `runtime.resolve(token, id)` requiring `Resolved::Note`; resolve routes through `ensure_namespace`, so foreign and absent both yield `None` | `merge_note_cross_namespace_either_id_returns_not_found` — PASS; `ensure_note_kind_rejects_foreign_note_before_kind_check` — PASS | RESOLVED |
| #568 | Traversal entry-points (`neighbors_with_query`, `traverse`, `bfs_traverse`, `shortest_path`) accepted caller-supplied root UUIDs without verifying namespace membership | Guard each root via `self.substrate_exists_in_ns(token, id)` at `operations.rs:857`, `operations.rs:884-890`, `graph_traversal.rs:71`, `graph_traversal.rs:146`; filter or return early if foreign | `traverse_foreign_namespace_root_yields_no_expansion` — PASS | RESOLVED |

## Cargo Gate Results (round 2)

```
cd crates && cargo check --workspace
→ Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.42s  (zero errors/warnings)

cd crates && cargo test --workspace
→ all crates: 0 failed (khive-runtime 322 unit + 26 integration; khive-pack-kg 174 unit + 112 integration)
```

## Commits (on `show/khive-issue-sweep/rt-security`)

```
71efd72 fix(pack-kg): route ensure_note_kind through resolve, close note oracle (#567)
0f31686 fix(runtime): get_edge foreign-ns returns None, close existence oracle (#548)
1fbabeb refactor(runtime): route remaining note-path namespace checks through ensure_namespace (#569)
1a0f894 chore: add implementation status artifact for o4 (#569 #548 #567 #568)
3de5b30 test(runtime): regression tests for namespace isolation fixes (#548, #567, #568)
31e5efa security(runtime): verify traversal roots before expansion (#568)
1b83cb7 security(runtime): namespace-check both merge_note ids before merge (#567)
50abfdb fix(runtime): ensure_namespace on get_edge (#548)
a604d23 refactor(runtime): centralize namespace check in ensure_namespace helper (#569)
```

## Skip Flags

None. All four issues fully resolved. No ADR-002, DB schema, or public API signature changes.
`get_edge` signature kept as `RuntimeResult<Option<Edge>>`; foreign IDs return `Ok(None)`.

## khive Usage

| Verb | Purpose | ID / Result |
|------|---------|-------------|
| `stats()` | Orient — entity/edge/note baseline | entities=2475, edges=7540, notes=11577 |
| `memory.remember` (semantic, salience=0.85) | Record ensure_namespace location, call sites, test counts | `c94cef35` |
| `memory.remember` (episodic, salience=0.75) | Record substrate_exists_in_ns traversal guard location | `9987696d` |
| `memory.remember` (episodic, salience=0.8) | Record oracle gap fixes (#548 Ok(None), #567 pack resolve) with error format note | `21cc3cc9` |

Upstream fix_brief.md consumed from `../an/fix_brief.md` (analyst o3).
Consumers: tester (o9), critic (o10).
