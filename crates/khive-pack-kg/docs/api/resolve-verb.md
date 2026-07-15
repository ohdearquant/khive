# The `resolve` verb

Technical reference for `resolve` — turning a caller-supplied natural-language or id-like
`ref` into an entity id, via a layered resolution pipeline. Handler is thin
(`handlers/resolve.rs`); all resolution logic lives in
`khive_runtime::reference_resolution`.

## Handler shape

`resolve`'s handler deserializes params, calls the runtime's `resolve_reference` capability
once per ref, and renders each `ReferenceResolution` to its wire shape. The handler performs
no mutation and no side effect beyond the ring reads/admissions `resolve_reference` and the
dispatch boundary already make.

`resolve`'s pipeline (id-string passthrough, ring, exact-name storage lookup, hybrid search)
is entity-only, so `kind` follows the same substrate-or-granular discriminant as
`create`/`list`/`search` (see `resolve_kind_spec`): the bare substrate label `"entity"` means
"no kind filter", not a literal `entities.kind` value. Forwarding the raw string as-is would
filter every real match out (#849) — `entities.kind` only ever holds a granular value like
`"concept"`, never `"entity"`.

## Resolution stages, in order

1. **Id-string passthrough** — a UUID ref resolves at confidence `1.0` regardless of whether
   the entity was ever touched through this registry's ring. Entity-only: a note's id-string
   is `NotFound` through `resolve`, even though `get` on the same id would succeed.
2. **Recently-referenced ring** — the dispatch boundary (`VerbRegistry::dispatch_with_identity`
   in `pack.rs`, not `KgPack::dispatch`) admits ids under their name whenever `create`/`get`/
   `update`/`delete`/`merge`/`link` runs through the registry. A later `resolve(refs=[name])`
   by the SAME actor resolves via the ring without running hybrid search. `search` result sets
   never admit to the ring (gate condition, 2026-07-09) — an entity that only ever went
   through `search` cannot resolve via the ring's high-confidence exact-match stage; ring
   admission and lookup are keyed on the same namespace (the gate-resolved `ns`, not
   `token.namespace()`), so this holds even under a non-local `default_namespace`. Two ring
   entries sharing a name resolve as `Ambiguous`, never a silent pick (F7 of the unified-verb
   draft ADR).
3. **Exact-name storage lookup** — an entity that exists but was never referenced through
   this registry's ring resolves via a direct storage lookup at `EXACT_NAME_CONFIDENCE`
   (0.98): above the ring's bands (0.95 exact-match / 0.7 substring-match), below the id-string
   passthrough's absolute 1.0. Unicode-safe and untokenized (CJK and embedded spaces resolve
   at full confidence); case-sensitive (a case-only variant falls back to hybrid search, not
   0.98); respects a granular `kind` filter (disambiguates same-name entities of different
   kinds instead of `Ambiguous`); invisible to soft-deleted entities (`deleted_at IS NULL` is
   baked into `query_entities`); two entities sharing an exact name resolve as `Ambiguous`,
   mirroring the ring's contract.
4. **Hybrid search fallback** — a ref with no exact-name match falls through to hybrid search
   at a confidence below the exact-name stage's 0.98. A ref matching nothing at any stage is
   `NotFound`.

`resolve`'s `kind` param is entity-only: a note kind is rejected with a clear error rather
than silently over-filtering to zero matches. `resolve` is registered as a public verb and
appears in `verbs()`.

## Test fixture notes

- Ring-admission-boundary tests go through `registry.dispatch(verb, params)` (not the direct
  `pack.dispatch(verb, params, &registry, &tok)` bypass most other tests in this module use)
  specifically so the admission hook at the `dispatch_with_identity` boundary fires.
- Exact-name-storage-lookup fixtures are created directly on the runtime (bypassing
  `registry.dispatch`, hence bypassing ring admission entirely), so the only way `resolve` can
  find them is the exact-name storage lookup (or, for the fallback-preserved case, hybrid
  search).
