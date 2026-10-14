# ADR-202: Read-only pack validators in `kkernel kg validate`

**Status**: Proposed\
**Date**: 2026-10-14\
**Issue**: #4783\
**Amends on acceptance**: ADR-034 §1 and §4 (policy selection), §8 (pipeline), and §9 (pack runner and corpus snapshot)\
**Grounding commit**: `3f21540c81696df785b03710f8c397eb775b7bc7`

[ADR-034](ADR-034-kg-validation-pipelines.md) remains accepted. This proposal changes only the policy-selection and read-only pack-runner decisions named above, upon acceptance. It does not supersede that ADR as a whole. The existing deferred runner remains deferred until this follow-up is accepted.

## Context

ADR-034 §9 ships a Rust callback API but explicitly requires a follow-up ADR before `kkernel kg validate` runs pack rules against corpus data. Issue #4783 tracks that deferred runner. Current `GraphSnapshot` contains only entity and edge counts. Both it and `ValidationContext` are non-exhaustive and have no public constructors. They cannot support the issue's missing-property acceptance rule.

`VerbRegistry::all_validation_rules` already collects static pack rule slices. The CLI uses a metadata registry for taxonomy and edge rules, but `PackMetadataRegistry` lacks a validation-rules accessor. Current structural and TOML passes use the shared evaluator report types; runtime callback findings use different types. The CLI summary counts failed rules by scalar severity, whereas runtime callbacks may emit mixed finding severities and edge targets.

## Decision and boundary

On acceptance, add a read-only runner for compiled Rust pack validators. It uses the existing registry collection, an immutable captured corpus, and the existing CLI report/exit path. It introduces no executable-script/plugin loading, production environment injection, storage reader/writer route, backend, sink or graph patch implementation.

All API signatures below are proposed additions. They do not describe shipped code. #4784 remains a later, separately gated write-path task. This runner never invokes `FixFn`, constructs/applies `GraphPatch`, or treats a pack callback as an auto-fix implementation.

## 1. One captured input set and a pure shared view

The host reads required `entities.ndjson` and `edges.ndjson`, and optional `notes.ndjson`, once for an invocation. It retains each input's readability/absence outcome and its immutable UTF-8 contents. All structural, TOML and pack validation results refer to that same captured set; a structural verdict cannot gate pack callbacks over a later reread of different bytes. Existing path-shaped test adapters may remain, but production checks must consume the captured inputs.

This is an immutable captured set, not an atomic multi-file filesystem revision. A concurrent edit during capture can yield records from different file revisions; all passes nevertheless validate the same captured bytes. Edits after capture do not affect that invocation. No snapshot timestamp, storage transaction, filesystem lock or measured isolation claim is added.

The existing pure lower `khive-rule-evaluator` crate owns a public decoder and opaque, owned read-only entity/edge/note views. The CLI must not copy an evaluator-private parser or introduce its own second NDJSON decoder. Existing TOML evaluation and the new decoder share the existing canonical record and edge-endpoint spelling rules; this proposal creates no new alias grammar or core schema relaxation.

Proposed API shape:

```rust
pub fn capture_graph(
    state: NdjsonState<'_>,
) -> Result<CapturedGraph, CorpusCaptureErrors>;

impl CapturedGraph {
    pub fn entities(&self) -> &[EntityView];
    pub fn edges(&self) -> &[EdgeView];
    pub fn notes(&self) -> &[NoteView];
}
```

`CapturedGraph` and its views have private owned data and immutable accessors only. Entity/note views expose their existing ID and kind, optional name where present, `properties()` as an optional borrowed JSON object, and the full borrowed record object for existing non-property fields. An absent or null properties field is distinguishable from an empty object. Edge views expose existing source/target/relation, optional edge ID, optional properties and the full record. The decoder preserves values without synthesizing IDs, normalizing property values, dropping fields or giving callbacks storage/file handles.

The decoder reports stream/line/reason for malformed JSON, non-object rows, incompatible field shapes or ambiguous duplicate target IDs within a substrate. It does not produce a partial-success `CapturedGraph`. Whitespace-only lines are ignored. Taxonomy, schema, references and existing structural policy remain the structural pass's responsibility; callbacks receive a graph only after that pass has no error-severity failure. Diagnostic text and count units for existing structural/TOML rules remain compatible.

The required input failure behavior remains ADR-034's: unreadable/missing mandatory inputs fail `required-input-files`; absent notes mean an empty note view, while present unreadable notes fail that rule. `summary.empty` continues to count nonblank captured input records independently of the verdict, including malformed records. Readable empty required inputs are valid and do not by themselves block callbacks.

## 2. Public runtime constructors and callback compatibility

`khive-runtime::validation` owns the callback-facing snapshot/context and uses the pure evaluator's captured views. This adds a runtime dependency on the existing lower evaluator. The lower evaluator must remain independent of runtime/packs/storage; no cycle or reverse dependency is introduced.

Proposed additions:

```rust
impl GraphSnapshot {
    pub fn new(corpus: khive_rule_evaluator::CapturedGraph) -> Self;
    pub fn entities(&self) -> &[khive_rule_evaluator::EntityView];
    pub fn edges(&self) -> &[khive_rule_evaluator::EdgeView];
    pub fn notes(&self) -> &[khive_rule_evaluator::NoteView];
}

impl<'a> ValidationContext<'a> {
    pub fn new(
        snapshot: &'a GraphSnapshot,
        config: &'a BTreeMap<&'static str, serde_json::Value>,
    ) -> Self;
}
```

The snapshot constructor derives the existing entity/edge counts from its views; it also exposes note count through an accessor. Existing count field reads, `RuleFn = fn(&ValidationContext<'_>) -> Vec<Violation>`, `ValidationRule`, and the default empty `PackRuntime::validation_rules` remain usable. The CLI holds the constructed snapshot immutably for the complete sequential runner. Callbacks receive only shared references.

V1 supplies one **empty** config map. There is no per-pack TOML config, enabled-rule list, inherited config or severity-override merge. The future-oriented context docs must explicitly describe that v1 restriction. The shipped TOML schema and parser remain authoritative; unknown future fields must not be accepted as if this proposal implemented them.

Runtime findings gain an explicit optional note target and public edge/note target builders alongside `with_entity`. These are additive proposed API changes; the existing non-exhaustive finding type remains non-exhaustive.

## 3. Metadata registry and rule identity

Add `PackMetadataRegistry::all_validation_rules(&self) -> Vec<&'static ValidationRule>` as a thin delegation to the existing `VerbRegistry::all_validation_rules`. Reuse `RuntimeConfig::for_metadata_registry`, discovered factories, `register_packs`, and `build_metadata`; do not activate a serving registry to obtain rules. The flattened collector is not reimplemented.

Before invoking any callback, validate the complete collection:

- Every ID has exactly one `/`, a nonempty prefix and nonempty rule component, and no whitespace/control characters.
- The prefix exactly matches a loaded pack name; there is no case folding or namespace rewriting.
- Full IDs are unique across the complete collection and must not collide with an existing structural/TOML result ID. Collisions refuse the pack runner even when callbacks are identical. This gate does not rewrite or newly reject legacy structural/TOML results by themselves.

A refusal emits one reserved unprefixed `pack-validation-registry` error result with deterministic diagnostic findings, invokes no pack callback, and exits nonzero through the normal report path. Structural/TOML findings are still reported unless the existing TOML syntax/format exit-2 contract applies.

**Attribution limit:** the current flattened API loses the contributing pack/owner pairing. Matching a loaded prefix proves only that the namespace exists, not that the contributing pack owns it. Compiled pack authors must declare their own namespace; v1 does not falsely claim that the runner authenticates ownership. Enforced contributor provenance would require a separate registry contract extension.

## 4. Pipeline, input gate and default policy

ADR-034 §8 is amended on acceptance to run:

1. Existing structural checks, always, with their existing order.
2. Existing TOML RulePass, when its file exists and `--no-rules` is false, in file order.
3. Compiled pack validators, enabled by default independently of TOML file presence, unless `--no-rules` is true.

This explicitly refines ADR-034's earlier absence-of-TOML promise: absence skips TOML policy, while compiled pack policy still runs. `--rules` selects TOML only. `--no-rules` skips both TOML and pack policy and retains all built-ins. It does not bypass structural errors. On acceptance, update ADR-034 §1, §4, §8 and §9 to state this narrow policy and runner change; other ADR-034 contracts remain in force.

TOML parse/unsupported-format errors retain exit 2 and their existing diagnostics. They do not invoke pack callbacks. Structural errors still do not abort the TOML pass.

When pack rules exist and are enabled, any error-severity structural failure, unreadable/incomplete captured input or capture/decode refusal prevents **all** pack callback execution. Emit a reserved `pack-validation-input` error result identifying the uncomputable corpus; do not emit successful results for skipped pack rules. Sort-order warnings alone do not block callbacks. An enabled collection with zero rules adds no synthetic success/failure result.

Readable empty valid data is computable: callbacks run against empty views. Their own policy may emit global findings. `summary.empty` remains true independently of those findings and the exit code.

## 5. Finding adaptation, severity and targets

Invoke each valid rule once. A finding must report the invoked rule's exact ID. A finding target must identify exactly one captured record in its declared substrate; global findings have no target. At most one of entity/edge/note target fields is set. An edge without an existing edge ID can receive a global diagnostic identifying its endpoints in the message; the runner never manufactures a target ID.

Wrong rule IDs, conflicting target fields, absent/ambiguous target IDs or otherwise unrepresentable findings refuse that callback's output. Emit deterministic findings in one reserved `pack-validation-output` error result and do not silently relabel, drop a target or report the callback as passed. Other independent callbacks may still run against the immutable snapshot.

The shared CLI finding type gains additive optional `edge_id` and `note_id` fields with omission when absent, preserving existing JSON output for existing findings. Entity IDs remain entity IDs. JSON preserves each pack finding's exact rule ID, severity, message and target substrate/ID. Text explicitly includes severity and target substrate; GitHub annotations use each finding's severity, with info as notice. Existing structural/TOML rendering remains compatible.

For each successfully adapted pack rule, produce one `RuleResult` with the namespaced ID. `passed` means it emitted no findings. For a failing rule, scalar severity is the maximum emitted severity (`error > warning > info`), irrespective of its declared default. For an empty passing rule, use the declared default severity as metadata. Every individual finding keeps its own severity.

Summary **units remain failed rules**, not finding rows: each failed result contributes once at its scalar severity. A mixed error/warning rule therefore contributes one error result, while both findings remain visible. Existing structural/TOML totals are unchanged. Ordinary success requires no failed error results; `--strict` also rejects failed warning results. Info alone remains successful, including under strict.

Pack findings are operationally non-fixable in this runner (`fixable: false`). A pack's declared callback/finding fix metadata does not enable a write path. Pack results never arm the existing `apply_fixes` trigger; that trigger retains its existing structural/TOML eligibility. Existing built-in `--fix` behavior is unchanged. No `FixFn` is invoked even when it is declared or `--fix` is present.

## 6. Determinism and trusted native callbacks

Keep structural order and TOML file order. Sort pack rules by exact ID using Rust string ordering. Expose each substrate's decoded records in stable ID order with captured line as a tie-breaker; ambiguous target IDs are refused before callbacks. Preserve opaque property values. Sort each callback's findings by target substrate/ID, severity, message and remaining rendered fields. Output-refusal diagnostics and gate findings use stable rule/stream/line order. V1 uses sequential invocation and no concurrency, retry or timeout mechanism.

Compiled native callbacks are trusted by compilation. Their contract is read-only and deterministic: no graph/file/network writes, no clock/random-dependent findings, no mutation of shared state, and no live runtime/backend access. The callback receives no I/O capability through this API, but native Rust can still access process globals and I/O APIs. **Metadata mode is not a sandbox and does not establish an I/O-capability guarantee.** This ADR provides no isolation proof for arbitrary native code and no containment for a hostile callback. Pack review must enforce the trusted-code contract.

A panic/abort is an execution failure, never a passing validation result. V1 does not promise recovery or a complete report after native process failure. Normal computable callbacks use the existing 0/1 verdict semantics; TOML syntax retains exit 2. No measured performance, statistical confidence or universal I/O-freedom claim is made.

## 7. Required executable proof

Add an off-by-default `kkernel` feature tentatively named `test-pack-validation`. It compiles a fixture pack/factory into the **actual child binary**, using the normal inventory/discovery/metadata path. Default production builds exclude that fixture. No environment variable, production pack selection override or dynamic callback/plugin injection is added. In-process registration alone cannot meet acceptance, since a child binary does not inherit the parent test registry.

The fixture pack contributes `validation-fixture/required-taxa-rank` with warning severity. Its native callback consumes the snapshot's entity property views and flags a concept whose `taxa_rank` is absent. The fixture uses private TempDirs, valid canonical IDs, required readable entity/edge files, and optional notes. The native suite must execute `CARGO_BIN_EXE_kkernel`, not only a helper or report comparator.

Required hot/cold proof:

- Hot: two concept entities, exactly one missing the property, no TOML file. Ordinary invocation reports exactly one targeted namespaced warning and exits 0. `--strict` reports it and exits 1. Assert text ID/severity and exact JSON ID/severity/entity target/message.
- Cold: add the property to that entity; strict invocation returns no violations for the fixture rule and exits 0. The fixture rule's passing result remains present, proving execution/collection.
- Disconnected-runner denial: acceptance assertions require the fixture rule result and targeted hot violation, not merely an exit code. A missing collection or skipped callback must fail the hot/cold proof. As an executed negative control, the same child with `--no-rules` produces a structural-only report; the hot acceptance validator must reject that report as evidence of a working runner. That control adds no production bypass.
- Preserve input bytes before/after each child invocation. Assert that pack fix callbacks never run, including under `--fix`, and that pack fixability alone cannot trigger built-in writes.

The executed suite must additionally cover: absent vs empty TOML policy; structural failures and malformed/partially decodable input blocking every callback; optional absent notes; present unreadable notes; readable empty graph; duplicate/invalid namespace IDs and cross-pass ID collisions; wrong callback rule ID/target; stable repeated output; mixed error/warning/info findings; entity/edge/note target preservation; and preservation of existing structural/TOML diagnostics, summary count units and exit-2 behavior. Registry/adapter unit fixtures may supplement these child-binary tests but cannot substitute for them.

## 8. Adoption, scope and gates

The proposal adds reciprocal pointers at ADR-034 §8/§9 and an ADR index entry. Acceptance also requires the policy-selection amendments at §1/§4. Likely later implementation owners are the lower evaluator's capture/views and shared report additions; runtime `validation.rs`, metadata accessor and Cargo dependency; CLI captured-input adapters/runner; and unit/integration fixture composition. That map is a planning boundary, not a source claim or authorization. No GraphPatch/backend/writer/sink work belongs here.

The new gate evaluates formerly uncomputable corpora/pack callbacks and changes the allowed invocation set by enabling compiled pack policy without TOML. Acceptance therefore requires adversarial design assessment of the input, registry and callback-output gates, including executed hot/cold and must-deny controls. This Proposed record does not report such an assessment as completed.

Acceptance requires the completed adversarial assessment, executed binary-suite evidence for §7, and an explicit architecture ratification against the final document revision. The proposal and planned tests alone are insufficient evidence. An isolated proof implementation may supply the required evidence before ratification; it does not authorize merging the deferred runner. Dependent implementation merges remain blocked until acceptance. #4784's write semantics remain outside this decision.

On acceptance, update ADR-034's affected sections and the ADR index without changing its accepted status or marking the whole ADR superseded. This proposal becomes the follow-up its §9 requires only after the evidence and ratification above.

## Evidence and alternatives

This proposal is grounded in the existing [runtime callback types](../../crates/khive-runtime/src/validation.rs), [registry collector](../../crates/khive-runtime/src/pack/catalog.rs), [metadata builder](../../crates/khive-runtime/src/pack/builder.rs), [CLI validation entry point](../../crates/kkernel/src/kg/validate.rs), and [shared pure evaluator](../../crates/khive-rule-evaluator/src/lib.rs) at the grounding commit. It specifies future behavior and reports no implemented API or measured result.

A counts-only snapshot cannot inspect missing properties. A private CLI parser would duplicate a shared lower-layer concern. An activated registry would widen the validation host unnecessarily. In-process-only or comparator-only tests cannot prove actual binary wiring. Flattening findings to declared default severity can allow an emitted error to pass; relabeling a note/edge ID as an entity corrupts target semantics. Those alternatives do not satisfy this proposal's contract.
