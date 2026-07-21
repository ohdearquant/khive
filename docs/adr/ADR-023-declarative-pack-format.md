# ADR-023: Pack Verb Surface, Visibility, and Composition

**Status**: accepted (supersedes the original ADR-023 "Declarative Pack Format")
**Date**: 2026-05-23
**Authors**: khive maintainers

## Context

The original ADR-023 framed declarative packs as YAML `pack.yaml` manifests parsed by a
Deno-side loader. That model has been **rescinded**.

Per the kkernel binary topology ([ADR-003](ADR-003-system-architecture.md),
[ADR-026](ADR-026-rust-binary-packaging.md)):

- `kkernel` is a Rust admin binary distributed as per-platform pre-compiled npm
  subpackages.
- The **pack handler trait** is the stable Rust ABI between kkernel and packs.
- All packs — built-in and third-party — are Rust crates implementing that trait.
- The user-facing `khive` CLI is an npm wrapper around the pre-compiled kkernel
  binary; it does not own pack semantics or implement a parallel pack-loader.

[ADR-017](ADR-017-pack-standard.md) already defines the foundational `Pack` and
`PackRuntime` traits. This ADR layers on:

1. The **agent verb surface** — what an agent sees through `request("pack.verb(...)")`.
2. The **handler vs. verb visibility** split — which handlers are MCP-exposed, which
   are kkernel-CLI / admin only.
3. The **composition model** — `KindHook` is the only extension mechanism; no
   overrides, no middleware in v1.
4. **Verb naming rules** — kg pack = bare verbs; all other packs prefix; sub-variants
   snake_case under the pack namespace.
5. **Field naming conventions** for verb parameters — full words, context-portable.
6. The **pack template / scaffold** pattern — cargo-generate from
   `khive-pack-template`, no proc macros, no DSL.

### Canonical handler trait shape

ADR-023 defines the canonical pack handler trait shape: `HandlerDef` with
`visibility: Visibility`, and `Pack::HANDLERS: &'static [HandlerDef]`.
ADR-017 (Pack Standard) consumes this trait shape. Any text in ADR-017 or other
ADRs that references `VerbDef` or `VERBS` is superseded by `HandlerDef` / `HANDLERS`.

### Scope

In scope: agent-facing verb surface, handler visibility, composition semantics, naming
rules, the pack template.

Not in scope: the foundational `Pack` / `PackRuntime` traits themselves (ADR-017), the
storage placement model (ADR-028), inventory-based discovery (ADR-027), or response
serialization and rendering format (governed by ADR-045 `PresentationMode` and the ADR-078
`format` axis).

**Note on response format (ADR-078)**: The `request` tool wire envelope accepts a `format`
parameter (`json` | `auto` | `table`) that selects the output serialization strategy.
When `format` is not `json`, the response payload is a rendered non-JSON string (markdown table
or flat key-value block) rather than a JSON structure. `format=json` is the canonical lossless
machine-readable form and the required choice for any caller that parses the response
programmatically. Pack handlers always return `Result<serde_json::Value, _>`; they have no
awareness of the `format` axis.

## Decision

### 1. Pack = Rust crate implementing the kkernel handler trait

Every pack is a Rust crate that:

1. Declares a `Pack` impl — static metadata (`NAME`, `NOTE_KINDS`, `ENTITY_KINDS`,
   `HANDLERS`, `REQUIRES`, `EDGE_RULES`).
2. Declares a `PackRuntime` impl — async dispatch.
3. Registers itself via the `inventory` crate ([ADR-027](ADR-027-dynamic-pack-loading.md))
   so kkernel discovers it at link time.

No YAML manifests, no Deno-side parsers, no IPC-based pack loading. The kkernel binary
links every pack as a Rust dependency; the linker collects all `inventory::submit!`
registrations into a single table at build time.

A pack contributes three things:

- **Vocabulary** — entity kinds, note kinds, allowed edge endpoint rules (additive only)
- **Handlers** — async functions invokable via the dispatch API; the full implementation
  surface
- **Verbs** — the _subset_ of handlers opt-in surfaced as MCP-callable verbs

### 2. Handlers vs. verbs — two-tier visibility

`Pack::HANDLERS` declares the full implementation surface. Each handler carries a
visibility tag:

```rust
pub struct HandlerDef {
    pub name:        &'static str,
    pub description: &'static str,
    pub visibility:  Visibility,
}

pub enum Visibility {
    /// Exposed to MCP agents. Agents call via `request("pack.verb(args)")`.
    Verb,
    /// Not on the MCP wire. Callable only via `kkernel exec '<pack>.<handler>(args)'`.
    /// Examples: `memory.recall_score`, `memory.recall_embed` — addressable
    /// through batch DSL chains but NOT registered as top-level MCP verbs.
    Subhandler,
}
```

The MCP transport filters by `visibility == Verb` when building the agent capability
list. `Subhandler` handlers are unreachable from MCP — agents never see them.

The kkernel CLI exposes the **full** handler surface via the verb-DSL `exec` subcommand:

```bash
kkernel exec 'memory.recall(query="...")'                      # Verb       — also on MCP
kkernel exec 'memory.recall_embed(query="...")'                 # Subhandler — admin only
kkernel exec 'memory.recall_fuse(query="...", limit=5)'         # Subhandler — admin only
kkernel exec 'memory.recall_score(rrf=0.4, salience=0.8, decay_factor=0.02, age_days=7)'  # Subhandler — admin only
```

The same handler is reachable as `memory.recall` from MCP and as
`kkernel exec 'memory.recall(...)'` from the CLI. The CLI is the operator's window into the
full surface; MCP is the agent's filtered view.

This replaces the previous `VerbDef` type. Migration is mechanical: rename
`VerbDef` → `HandlerDef`, add `visibility: Visibility::Verb` to every existing
verb entry in pack-kg / pack-gtd / pack-memory / pack-brain crates, and mark
internal pipeline handlers with `Visibility::Subhandler`.

### 3. Operator visibility override status

Per-pack `verbs_disabled` configuration is **not supported in the shipped v1
operator surface**. The active surface has two controls:

1. Pack authors set each handler's static `Visibility::{Verb, Subhandler}` in
   `Pack::HANDLERS`.
2. Operators select which packs load via ADR-027 pack selection (`--pack`,
   `KHIVE_PACKS`, or the built-in production default pack set).

The runtime's MCP capability list is built from loaded handlers whose static
visibility is `Visibility::Verb`. There is no deployment-time downgrade path from
`Verb` to `Subhandler`, and operators cannot promote `Subhandler` handlers onto the
MCP wire.

`verbs_disabled` remains a deferred policy hook. Implementing it requires a config
parser field, boot-time validation against loaded pack handlers, and tests proving
that disabled verbs disappear from MCP capability discovery while remaining
available to operator-only introspection.

### 4. Verb naming — kg bare, all others pack-prefixed

The native kg pack (`khive-pack-kg`) owns the **substrate verbs** and exposes them as
bare verb names (18 verbs total):

| Verb        | Speech act  | Description                                                                                                                                                                                                                                                                                                                                                                                     |
| ----------- | ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create`    | commissive  | Create an entity or note. Bulk shape: pass `items` (array of entity specs, `kind` + `name` required per item, capped at 1000) instead of a top-level `kind`; `atomic` (default `true`) controls all-or-nothing vs. best-effort per-item semantics, and `verbose` includes the created entity objects in the response. Bulk-created entities skip vector embedding until a subsequent `reindex`. |
| `get`       | assertive   | Fetch any record by UUID                                                                                                                                                                                                                                                                                                                                                                        |
| `list`      | assertive   | Structured browse with pagination                                                                                                                                                                                                                                                                                                                                                               |
| `update`    | declaration | Patch entity or edge fields                                                                                                                                                                                                                                                                                                                                                                     |
| `delete`    | declaration | Soft or hard delete a record                                                                                                                                                                                                                                                                                                                                                                    |
| `search`    | assertive   | Hybrid FTS + vector search                                                                                                                                                                                                                                                                                                                                                                      |
| `link`      | commissive  | Create a typed directed edge                                                                                                                                                                                                                                                                                                                                                                    |
| `neighbors` | assertive   | Immediate graph neighbors; optional `include_entity_type` param enriches each hit with its entity subtype                                                                                                                                                                                                                                                                                       |
| `traverse`  | assertive   | Multi-hop BFS traversal; optional `include_properties` param enriches each path node with entity properties                                                                                                                                                                                                                                                                                     |
| `query`     | assertive   | GQL/SPARQL pattern matching                                                                                                                                                                                                                                                                                                                                                                     |
| `merge`     | declaration | Deduplicate two entities                                                                                                                                                                                                                                                                                                                                                                        |
| `propose`   | commissive  | Create a proposal for KG mutation; emits `ProposalCreated`.                                                                                                                                                                                                                                                                                                                                     |
| `review`    | declaration | Record an approve/reject decision on an open proposal; emits `ProposalReviewed`.                                                                                                                                                                                                                                                                                                                |
| `withdraw`  | commissive  | Rescind an open proposal (proposer-only); emits `ProposalWithdrawn`.                                                                                                                                                                                                                                                                                                                            |
| `stats`     | assertive   | Aggregate counts and health metrics for the namespace graph.                                                                                                                                                                                                                                                                                                                                    |
| `verbs`     | assertive   | Enumerate all MCP-callable verbs; supports `category` and `pack` filters.                                                                                                                                                                                                                                                                                                                       |
| `context`   | assertive   | Entity-anchored graph context in one call: resolves anchors (by ID or hybrid search), expands 1-2 hops with per-node fanout caps, and packs the result within a char budget (ADR-089).                                                                                                                                                                                                          |
| `resolve`   | assertive   | Resolve natural-language references to record ids: id-string passthrough, recently-referenced ring, exact-name match, then hybrid-search fallback; returns Resolved/Ambiguous/NotFound per ref, never a silent pick.                                                                                                                                                                            |

`verbs` was added in Wave 4 (ue-help-introspection H5) to provide a machine-readable discovery
endpoint. It is a pure read operation with no side effects. It excludes internal subhandlers
(`Visibility::Subhandler`) from its output — the returned list is identical to what the
`request` tool's MCP description advertises.

`context` was added under ADR-089 (2026-07-04) as a bare kg-substrate verb — it composes the existing `search`/`neighbors` runtime ops (`hybrid_search`, `neighbors_with_query`) into a single call rather than introducing new storage or a new runtime operation, matching the "reuse runtime ops unchanged, compose in the handler" precedent already used by `traverse`.

Every other pack prefixes its verbs with the pack name and a single dot:

```
memory.remember, memory.recall, memory.recall_candidates, memory.recall_score
gtd.assign, gtd.next, gtd.complete, gtd.transition, gtd.tasks
brain.profiles, brain.profile, brain.resolve, brain.backtest, brain.compare,
brain.activate, brain.deactivate, brain.archive, brain.bind, brain.unbind,
brain.feedback, brain.merge_profiles,
brain.snapshot, brain.events, brain.emit, brain.config  ← non-normative example;
see ADR-032 for the authoritative brain verb table and visibility tags
```

Sub-variants of a pack verb use **snake_case within the pack namespace**, not nested
dots:

```
✓ memory.recall_candidates       one dot, pack-prefixed
✗ memory.recall.candidates       two dots, breaks "first dot is pack" rule
```

The single rule: **first dot is always the pack name. There is no second dot.**

**Enforcement**: This rule is enforced at CI time by the contract test at
`crates/kkernel/tests/verb_namespace_contract.rs`. The test loads every
`inventory`-registered pack, walks all `HandlerDef` names, and asserts:

- A bare name (no dot) must be in the 18-entry kg-substrate allowlist.
- A dotted name must carry exactly one dot whose prefix matches `Pack::NAME`.
- Two or more dots are always a violation.

Any verb name change or new pack registration that violates §4 will fail CI.

The kkernel CLI uses the same dotted verb-DSL form as MCP, via the `exec` subcommand:

```bash
kkernel exec '<pack>.<handler>(args...)'
```

So the same handler is `memory.recall_candidates` on MCP and
`kkernel exec 'memory.recall_candidates(...)'` from the CLI.

### 5. Field naming — full words, context-portable

Verb parameters use full words, not abbreviations, even when the verb name implies
context:

```
✓ memory.recall(memory_type="episodic", query="...", limit=10)
✗ memory.recall(mem_type="episodic")
✗ memory.recall(type="episodic")           # collides with entity_type / note_type

✓ gtd.assign(status="next", priority="p1")
✗ gtd.assign(task_status="next")           # rejected: deny_unknown_fields in handler
✗ gtd.assign(task_priority="p1")           # rejected: deny_unknown_fields in handler

✓ brain.feedback(target_event_id="...", score=0.8)
✗ brain.feedback(event_id="...")           # which event?
```

The verb prefix (`memory.recall`) gives the _agent_ call context, but the field name
travels into JSON payloads, audit logs, training data, and error frames where it loses
that context. Full-word field names carry their own meaning.

> **Implementation note**: `gtd.assign` uses `status` and `priority` directly.
> The param struct is compiled with `#[serde(deny_unknown_fields)]`, so
> `task_status` and `task_priority` are rejected at runtime rather than silently
> ignored — they are not aliases.

### 6. Substrate verbs are kind-polymorphic — KindHook on every substrate verb

The kg pack's 10 substrate verbs (`create`, `get`, `list`, `update`, `delete`,
`search`, `link`, `neighbors`, `traverse`, `query`) operate uniformly on the merged
vocabulary. When called with a `kind` argument, kg consults the owning pack's
`KindHook` for that kind.

[ADR-017](ADR-017-pack-standard.md) defines `KindHook` with `prepare_create` /
`after_create` only. **This ADR generalizes** to every substrate verb:

```rust
#[async_trait]
pub trait KindHook: Send + Sync + std::fmt::Debug {
    // create — already in ADR-017
    async fn prepare_create(&self, rt: &KhiveRuntime, args: &mut Value) -> Result<(), RuntimeError>;
    async fn after_create  (&self, rt: &KhiveRuntime, id: Uuid, args: &Value) -> Result<(), RuntimeError>;

    // list
    async fn prepare_list  (&self, rt: &KhiveRuntime, args: &mut Value)             -> Result<(), RuntimeError> { Ok(()) }
    async fn after_list    (&self, rt: &KhiveRuntime, args: &Value, hits: &mut Value) -> Result<(), RuntimeError> { Ok(()) }

    // search
    async fn prepare_search(&self, rt: &KhiveRuntime, args: &mut Value)             -> Result<(), RuntimeError> { Ok(()) }
    async fn after_search  (&self, rt: &KhiveRuntime, args: &Value, hits: &mut Value) -> Result<(), RuntimeError> { Ok(()) }

    // update / delete / link / neighbors / traverse / query — same shape, empty defaults
    // ...
}
```

All non-create hooks have empty default impls. Packs override only the ones they need.

Concrete examples:

- **gtd** hooks `prepare_create` for `kind="task"` to fill `status="inbox"` defaults,
  and `prepare_list` for `kind="task"` to sort by priority + due_date.
- **memory** hooks `after_create` for `kind="memory"` to compute and store the
  embedding alongside the note.

### 7. Composition rules

| Mechanism                                          | Status in v1                                                            |
| -------------------------------------------------- | ----------------------------------------------------------------------- |
| **KindHook extension** (vertical)                  | **Required.** The only way packs extend each other.                     |
| **Verb override** (pack B replaces pack A's verb)  | **Forbidden.** Boot-time collision → `BootError::VerbCollision`.        |
| **Middleware / wrap** (intercept any verb)         | **Not supported.** Auth (ADR-018), audit, rate-limit are runtime-level. |
| **Verb inheritance**                               | **Not supported.** `REQUIRES` is vocabulary + load-order only.          |
| **Horizontal sharing** (two packs, same verb name) | **Forbidden.** Same as override. Use distinct verb names.               |

Third parties extend behavior in exactly two ways:

1. **New pack-private verbs** — author a new pack with new verbs. Verb names cannot
   collide with any existing verb name (kg-bare or pack-prefixed).
2. **KindHook on existing substrate verbs** — register a `KindHook` for a new kind;
   the kg substrate verbs automatically route through it.

This keeps verb semantics predictable for agents: no surprise behavior swaps mid-call.

### 8. Pack template — cargo-generate from `khive-pack-template`

A reference template crate lives at `crates/khive-pack-template/` in the workspace.
Authoring a new pack is a single command:

```bash
cargo generate --git https://github.com/khive-ai/khive --template pack
> Pack name (snake_case, ≤16 chars): exp
> Pack description: ML experiment tracking
> Note kinds (comma-separated): experiment_log
> Entity kinds: training_run, hyperparameter_set
> Pack dependencies: kg
```

The generator produces a working Rust crate:

```
crates/khive-pack-exp/
├── Cargo.toml          # workspace-style, depends on khive-types + khive-runtime + inventory
├── src/
│   ├── lib.rs          # Pack + PackRuntime + PackFactory + inventory::submit!
│   ├── vocab.rs        # closed enums for declared kinds with FromStr aliases
│   └── handlers.rs     # stub for each declared verb with TODO!() bodies
└── tests/
    └── integration.rs  # smoke test invoking each verb
```

The author fills in `handlers.rs`. Everything else is generated boilerplate.

**No macros, no DSLs** — the generated crate is plain Rust. An LLM (or a human) can
read and modify the result directly; rust-analyzer works without expansion magic;
debugging shows real call stacks, not macro-expanded ones.

The canonical reference impl is `crates/khive-pack-kg/{lib.rs, vocab.rs, handlers.rs}`.
New pack authors should read it first.

### 9. Discovery and registration

Packs are discovered at link time via the `inventory` crate (per
[ADR-027](ADR-027-dynamic-pack-loading.md)):

```rust
// crates/khive-pack-exp/src/lib.rs
struct ExpPackFactory;

impl khive_runtime::PackFactory for ExpPackFactory {
    fn name(&self) -> &'static str { "exp" }
    fn create(&self, rt: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(ExpPack::new(rt))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&ExpPackFactory) }
```

kkernel iterates `inventory::iter::<PackRegistration>()` at startup, runs `REQUIRES`
topological sort, and registers each pack's handlers in the `VerbRegistry`. Boot-time
collision on a verb name = `BootError::VerbCollision { verb, packs: [...] }`.

### 10. Pack installation

For **built-in packs** (compiled into the kkernel binary): nothing to install. They
are link-time-discovered automatically.

For **third-party packs**: the pack author publishes their crate to crates.io. End
users do not "install" packs at runtime — there is no dynamic loading. To use a
third-party pack, build kkernel with that pack as a dependency.

This is a deliberate v1 design choice (dropping v0's YAML-pack-install model):

- Type safety and ABI stability across the kkernel/pack boundary outweigh the
  zero-rebuild ergonomics of dynamic packs.
- The npm distribution model (ADR-026) lets the kkernel team ship pre-built binaries
  with different pack sets (e.g. `khive-research`, `khive-pkm`, `khive-minimal`).
  Power users build their own.

A future ADR may revisit dynamic loading once a clear use case emerges. For v1: static
linking + npm-distributed variant binaries cover the spectrum.

## Rationale

### Why Rust crates, not YAML?

YAML manifests were appealing for "non-Rust devs can extend the vocabulary" ergonomics
but break the kkernel handler ABI invariant: kkernel and its packs share a stable
Rust trait surface, and the type system enforces correctness across the boundary. A
YAML manifest cannot declare a verb handler — only the vocabulary. Since vocabulary
extension without verb extension is rarely useful (every interesting pack we've seen
wants both), the YAML path is a half-feature that creates a second concept to
maintain.

The Rust-only path avoids:

- A second pack-format spec (the manifest) parallel to the Rust trait
- Two validation paths (YAML schema vs Rust compile errors)
- A runtime YAML parser inside kkernel
- "But can I add a custom KindHook to my YAML pack?" follow-up requests

The cargo-generate template (§8) makes the Rust path approachable: answer a prompt,
get a working crate, fill in handlers. The ceiling on what an author can build is far
higher than a YAML manifest could ever support.

### Why two-tier visibility, not finer?

**Agent documentation gating happens at the skill layer, not the visibility layer.** A
rarely-used Verb is still a Verb — it just isn't in every agent's daily skill manual.
If a handler is risky enough to hide from the agent's daily surface, it should be
`Subhandler` (CLI-only); a skill can explicitly opt in by surfacing the CLI invocation.

The Visibility tier answers "is this on the MCP wire at all?" — a binary question.
The skill layer answers "when should the agent use it?" — a recommendations question.
Conflating these into one taxonomy is a category error.

### Why no overrides / no middleware?

**Predictability for agents.** If `gtd` could override kg's `create` verb, then
`create(kind=task)` might behave differently depending on which packs are loaded in
this deployment. Agents would have to introspect the pack set before reasoning about
verb semantics.

KindHook is the safe extension point: the substrate verb's _contract_ (semantically:
"create a new record of kind X with these fields") is constant across deployments;
only the _defaults / side effects / derived properties_ differ per kind. That gives
packs real extensibility without breaking the agent's mental model.

Middleware (auth, audit, rate-limit) is a runtime concern, not a pack concern.
[ADR-018](ADR-018-authorization-gate.md) covers auth as a separate gate that wraps
the entire verb registry; audit can be plumbed similarly. Letting packs install
middleware would create unpredictable verb-level interception chains — even worse
than overrides.

### Why field-name full-words?

Three reasons:

1. **JSON / log portability.** Field names travel into error frames, audit logs, and
   training data, where the verb-call context is lost. `mem_type: "episodic"` in an
   error frame is harder to debug than `memory_type: "episodic"`.
2. **Disambiguation.** Bare `type` collides with `entity_type`, `note_type`, and the
   substrate-discriminator `kind`. Full-name fields avoid the collision.
3. **One rule for agents to learn.** "When in doubt, full-word the field" is a single
   rule. Abbreviation policies require their own style guide.

## Consequences

### Positive

- **Single ABI surface.** kkernel and packs share one Rust trait. No
  format-vs-code duality. Compile-time integration checks.
- **Predictable verb surface for agents.** Bare verbs ⇒ kg. Dotted ⇒ pack-prefixed.
  No verb is silently replaced. The MCP capability list is stable for a selected
  pack set and static handler visibility.
- **Clear extension story.** New pack? Cargo-generate from template, fill handlers.
  New behavior on an existing kind? KindHook. Risky handler? Visibility::Subhandler.
- **Auditable visibility.** The full handler surface is available to operator
  introspection; the MCP surface is the loaded subset whose handlers are declared
  `Visibility::Verb`.
- **No magic.** Plain Rust packs. Rust-analyzer works. Stack traces are real. LLMs
  can write packs by adapting the reference.

### Negative

- **Rust required for any pack.** Domain experts without Rust still cannot author
  packs directly. Mitigation: cargo-generate template + the reference pack-kg make
  the bar approachable. A domain expert can get most of the way by editing strings
  and filling `handlers.rs` stubs.
- **No runtime pack install.** Third-party packs require a kkernel rebuild. npm
  distribution mitigates by letting variants ship pre-built. For v1, acceptable given
  the audience (researchers, agent platforms).
- **`KindHook` trait surface grows.** Generalizing to all substrate verbs adds ~16
  methods (8 verbs × prepare/after pairs). Default impls keep this from being a
  burden for pack authors — they override only what they need.

### Neutral

- **VerbDef → HandlerDef rename.** Existing pack code needs a one-time migration:
  rename `VerbDef` to `HandlerDef`, add `visibility: Visibility::Verb` to every
  externally callable entry, and mark internal pipeline handlers with
  `Visibility::Subhandler`. Mechanical; tooling can do it in one sed pass.

## Alternatives Considered

| Alternative                                                   | Why rejected                                                                                                                                                                                             |
| ------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| YAML pack manifests (original ADR-023 v0)                     | Bifurcates the pack ecosystem into code-packs and data-packs. Half-feature: real packs want vocabulary + verbs. Increases maintenance load. The accepted decision keeps packs in Rust.                   |
| Pack verb override (last-loaded-wins or explicit declaration) | Breaks agent predictability. Verb semantics become deployment-dependent. KindHook is the safe extension point.                                                                                           |
| Pack middleware / wrap interception                           | Composes badly (order-dependent chains); auth and audit belong at the runtime gate (ADR-018), not the pack layer.                                                                                        |
| Four-tier visibility (Public/Advanced/Debug/Internal)         | Conflates MCP exposure with documentation gating. Skills already handle docs; two tiers (on/off the MCP wire) are sufficient.                                                                            |
| Proc macro (`#[derive(Pack)]`)                                | Adds expansion magic; rust-analyzer + debuggers have to expand. Cargo-generate produces plain Rust that any LLM/human reads directly.                                                                    |
| Declarative bang macro (`khive_pack! { ... }`)                | Same as proc macro; adds a DSL parallel to the real trait. Template + plain Rust is simpler.                                                                                                             |
| Operators can promote Internal → Verb in khive.toml           | Bypasses pack author's safety contract. Operator can disable but never expand the MCP surface.                                                                                                           |
| Allow new edge relation names from packs                      | Fragments traversal semantics; two packs may invent different names for the same concept. The closed 15-relation set (ADR-002) plus extensible endpoints (ADR-017 `EDGE_RULES`) covers the design space. |

## Implementation

### Trait surface (lives in `khive-types::pack`)

```rust
pub struct HandlerDef {
    pub name:        &'static str,
    pub description: &'static str,
    pub visibility:  Visibility,
}

pub enum Visibility { Verb, Subhandler }

pub trait Pack {
    const NAME:         &'static str;
    const NOTE_KINDS:   &'static [&'static str];
    const ENTITY_KINDS: &'static [&'static str];
    const HANDLERS:     &'static [HandlerDef];
    const EDGE_RULES:   &'static [EdgeEndpointRule] = &[];
    const REQUIRES:     &'static [&'static str]     = &[];
}
```

### Operator config status

`verbs_disabled` is deferred and is not part of the shipped `khive.toml` schema.
Current operator control is pack selection per ADR-027 plus pack-authored static
handler visibility.

### KindHook extension

Add `prepare_X` / `after_X` for each substrate verb. All non-create methods have
empty default impls so existing packs compile unchanged after the rename.

### Template crate

```
crates/khive-pack-template/
├── Cargo.toml.liquid
├── src/
│   ├── lib.rs.liquid
│   ├── vocab.rs.liquid
│   └── handlers.rs.liquid
└── cargo-generate.toml
```

`cargo generate --git https://github.com/khive-ai/khive --template pack` produces a
working crate. Reference impl: `crates/khive-pack-kg/`.

### Tests

| Scenario                                                       | Assert                                                      |
| -------------------------------------------------------------- | ----------------------------------------------------------- |
| Two packs declare the same verb name                           | `BootError::VerbCollision` at registration                  |
| Static `Visibility::Subhandler` handler in a loaded pack       | Excluded from MCP capability list                           |
| Substrate verb with kind-owning pack registering KindHook      | `create(kind=X, ...)` routes through prepare_create         |
| Subhandler invoked via MCP `request("pack.subhandler_x(...)")` | `RuntimeError::HandlerNotExposed`                           |
| Subhandler visible through operator introspection              | Listed as `Visibility::Subhandler`, not MCP-callable        |
| Future `verbs_disabled` config policy                          | Deferred; requires parser, validation, and capability tests |
| Pack template-generated crate compiles + passes smoke test     | Yes                                                         |

## Amendment: dispatch-by-kind + KindHook as the mandatory pattern for future packs (2026-07-05)

Per ADR-095 (F1/F3, verb-surface consolidation and field-validation governance), this
ADR is amended with two governance rules, documentation-only, with no change to the
current verb surface or verb count:

1. New packs express CRUD-shaped operations (create/read/update/delete over an entity
   or note kind) through the kg pack's dispatch-by-kind verbs (`create`, `search`,
   `list`, `get`, `update`, `delete`) rather than introducing bespoke pack-prefixed
   verbs for the same operation, unless the operation carries genuine non-CRUD domain
   logic (a state machine, an atomic multi-step guard, or side effects beyond storing
   a record) that dispatch-by-kind cannot express. Existing named verbs accepted for
   exactly this reason (for example the session pack's `session.resume`, per
   ADR-083) are unaffected; this rule governs new packs and new verbs going forward.
2. Per-kind create-time field validation (enum checks, format checks, cross-field
   guards) lives in that kind's `KindHook::prepare_create` implementation
   ([ADR-017](ADR-017-pack-standard.md) §KindHook), not in a parallel handler that
   duplicates the same checks outside the hook seam. This converges validation onto
   one site per kind and removes the divergence risk of the same rule being
   implemented twice.

This amendment does not retire, rename, or alias any verb. See ADR-095 for the full
argument, the rejected alternatives (wire-facing verb retirement, a composed
field-validation registry analogous to `EDGE_RULES`), and the accompanying internal
refactor that unifies `gtd.assign` onto the shared create-plus-`TaskHook` path.

## Amendment: reduced open-source pack surface (2026-07-20)

Effective with the accompanying crate-extraction changes (which land as
separate pull requests; this amendment describes the surface they produce
together), the open-source distribution's default pack set is reduced. The
brain, knowledge, code, and git packs move to commercially licensed
extensions maintained outside this repository; they are not part of the
open-source distribution. The git pack's departure takes its note kinds
(`commit`, `issue`, `pull_request`), its ingestion surface, and its verbs
with it. The workspace pack's declared pack dependency is accordingly
relaxed from `kg, git, gtd, session` to `kg, gtd, session`: its membership
rules targeting the git note kinds stay declared but are inert when no pack
registers those kinds (edge endpoint rules are installed without
kind-existence validation and can only ever match kinds a loaded pack
provides), so the reduced default boots and workspace containment continues
to serve task and session records. The formal pack, which was never part of the open-source default
pack set (see below), moves alongside the code pack as part of the same
crate departure. A small set of channel-transport crates supporting alternate
message delivery also move out of this repository; they contribute no MCP
verbs and are unrelated to the pack-verb surface this ADR governs.

Interface crates remain. `khive-brain-core` — and, in general, any interface
crate that a shipped pack depends on — stays in this repository:
`khive-pack-memory` has a hard dependency on `khive-brain-core` for the
ranking types and the degradation behavior described below. Removing a pack
crate never removes the interface crates the remaining open-source packs
consume.

### Default pack set, before and after

The open-source default pack set (`RuntimeConfig::default()`, selectable via
`KHIVE_PACKS` / `--pack`) was, before this change:

```
kg, gtd, memory, brain, comm, schedule, knowledge, session, git, code, workspace, blob
```

and is, after this change:

```
kg, gtd, memory, comm, schedule, session, workspace, blob
```

The formal pack was never included in this default set. It was compiled into
the admin (`kkernel`) binary for operator/CLI-only use and never registered as
part of the agent-facing MCP pack selection; its removal here changes which
crates ship in this repository, not the default verb surface.

### Consequence for the agent-facing verb surface

Following this change, the open-source build no longer registers `brain.*`,
`knowledge.*`, or `git.*` verbs, nor the `code.ingest` verb. The kg substrate
verbs (§4) and the `memory.*` verbs are unaffected and continue to operate as
documented.

Serving-profile resolution inside `memory.recall` degrades gracefully in the
absence of a registered brain pack **only when no profile is configured**:
profile resolution dispatches an internal `brain.resolve` call and treats a
failed dispatch (no such verb registered) the same as a
resolvable-but-unmatched profile, returning no serving profile. Recall
operates on its plain-scoring path; the profile-weighted ranking terms
described elsewhere in this document simply do not apply when no brain pack
is loaded.

That graceful path does not extend to configured or requested profiles. A
memory-pack configuration that names a brain profile bypasses `brain.resolve`
entirely: the configured profile id survives a failed `brain.profile` state
read (the handler logs a warning and scores with configured defaults) and is
still stamped as `served_by_profile_id` on recall results — the stamp then
reflects static configuration, not live profile state. A per-request
`profile_id` parameter is stricter still: its `brain.profile` dispatch failure
is a hard error, so the request fails outright with no brain pack registered.
Deployments of the open-source build should therefore leave the memory pack's
brain-profile configuration unset and omit per-request profile ids.

`memory.feedback` is not fully symmetric: when its configuration routes
feedback through a brain profile, the handler dispatches `brain.feedback`,
and with no brain pack registered that dispatch fails and the error
propagates to the caller. Deployments of the open-source build should not
configure brain-profile feedback routing; the plain feedback path operates
unchanged.

### Composition after the carve

This amendment does not change how packs compose. Per [ADR-027](ADR-027-dynamic-pack-loading.md),
each pack registers itself into the shared handler table via `inventory::submit!`
at link time; there is no plugin-loading step at runtime. A distribution that
wishes to compose additional packs — including the ones described above — does
so by adding their crates as build dependencies and force-linking them the same
way every pack in this repository is force-linked today (§9); the open-source
repository itself carries no feature flags or optional dependencies referencing
packs it does not ship. Runtime configuration sections that an extension pack
consumes are the one deliberate exception: the `[git_write]` allowlist plumbing
is retained (ADR-108 amendment of the same date) so a deployment that loads the
extension gets the documented fail-closed behavior without further changes, and
the section is inert — parsed but consumed by nothing — in a build where the
pack is not loaded. Retained inert configuration of this shape is permitted;
feature flags and optional crate dependencies on unshipped packs are not.

### Scope of this amendment

This amendment records the pack-surface consequence of moving pack crates out
of this repository. It does not introduce, retire, or rename any verb, and it
does not change the visibility, naming, or composition rules established
elsewhere in this ADR. Corresponding extraction changes are expected to land as
separate pull requests against this repository.

## Amendment: single-pack open-source surface (2026-07-20)

A second extraction, landing later the same day as the preceding amendment,
completes the reduction: the open-source distribution ships exactly one
production pack, `kg`, plus the `khive-pack-template` example pack. The
`gtd`, `memory`, `comm`, `schedule`, `session`, `workspace`, and `blob`
packs move to commercially licensed extensions maintained outside this
repository, joining the packs extracted by the preceding amendment.

The `khive-brain-core` interface crate moves out as well. This supersedes
the preceding amendment's "interface crates remain" rule in its specific
application: that rule was grounded in `khive-pack-memory`'s hard dependency
on `khive-brain-core`, and with the memory pack itself extracted, no pack in
this repository consumes the crate. The general principle stands — an
interface crate stays for as long as a shipped open-source pack depends on
it — and now selects the empty set.

### Default pack set, before and after

The open-source default pack set (`RuntimeConfig::default()`, selectable via
`KHIVE_PACKS` / `--pack`) was, after the preceding amendment:

```
kg, gtd, memory, comm, schedule, session, workspace, blob
```

and is, after this change:

```
kg
```

### Consequence for the agent-facing verb surface

The open-source build registers the kg substrate verbs (§4) and nothing
else. Regenerate the authoritative count via `request(ops="verbs()")`; at
the time of this amendment it is 18 verbs. Extension packs, when installed,
load through the same `KHIVE_PACKS` / `--pack` selection this ADR already
specifies — the reduction changes which crates ship in this repository, not
the selection mechanism.

To support distributions that link extension packs, the admin binary's CLI
entry point is exposed as a library function (`kkernel::cli::cli_main`): a
downstream distribution crate depends on this repository's crates, adds its
pack crates as dependencies with one force-link `use` per pack (the ADR-027
inventory registration requires the crate to be linked), and provides a
one-line `main`. No dispatch, registry, or configuration code is duplicated
downstream.

### Data in place

No schema or data migration accompanies this change. A database written by a
fuller pack set opens under the reduced default, and no stored data is
altered or removed. Substrate records that extension packs store as entities
and notes remain reachable through the substrate-level kg verbs to the
extent those verbs' own resolution rules cover them; pack-private auxiliary
state (for example profile tables) and content-addressed blob objects are
retained on disk but are not accessible until the owning pack — and with it
the pack's registered resolvers and verbs — is loaded again.

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md) — closed `EntityKind` taxonomy that packs
  extend via `entity_type`, not new kinds
- [ADR-002](ADR-002-edge-ontology.md) — closed 15-relation set; packs extend endpoints
  via `EDGE_RULES`, not new relations
- [ADR-003](ADR-003-system-architecture.md) — kkernel / khive split
- [ADR-013](ADR-013-note-kind-taxonomy.md) — closed note kind taxonomy
- [ADR-017](ADR-017-pack-standard.md) — foundational `Pack` / `PackRuntime` traits;
  this ADR layers visibility + verb naming + extended KindHook on top
- [ADR-018](ADR-018-authorization-gate.md) — runtime-level auth gate (not pack-layer
  middleware)
- [ADR-025](ADR-025-verb-speech-acts.md) — Searle's speech-act classification of verbs
- [ADR-026](ADR-026-rust-binary-packaging.md) — per-platform npm subpackages
- [ADR-027](ADR-027-dynamic-pack-loading.md) — inventory-based pack discovery
- [ADR-028](ADR-028-pack-scoped-backends.md) — `khive.toml` operator config
- [ADR-046](ADR-046-event-sourced-proposals.md) — Event-Sourced Proposals — source ADR for `propose`, `review`, and `withdraw` verbs
- [ADR-089](ADR-089-context-verb.md) — source ADR for the `context` verb (entity-anchored graph context)
- [ADR-095](ADR-095-verb-surface-consolidation.md) — verb-surface consolidation and field-validation governance; source of the dispatch-by-kind + KindHook amendment above

## Supersedes

The v0 ADR-023 ("Declarative Pack Format") — YAML-manifest model is rescinded per
Accepted decision (2026-05-23). Packs are Rust crates; vocabulary and verbs are declared together
in the pack trait.
