# ADR-027: Dynamic Pack Loading via Self-Registration

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

ADR-017 establishes packs as composable vocabulary + verb-handler bundles, declaring
`NAME`, `VERBS`, `NOTE_KINDS`, `ENTITY_KINDS`, and `REQUIRES`. The boot path must
construct the right packs based on configuration, respecting `REQUIRES` dependencies.

The historical implementation used a static dispatch crate:

```rust
// khive-dialect-kg (the regression site)
match name {
    "kg"     => builder.register(KgPack::new(runtime)),
    "gtd"    => builder.register(GtdPack::new(runtime)),
    "memory" => builder.register(MemoryPack::new(runtime)),
    _        => Err(name),
}
```

This worked at three packs. It does not scale. Brain (ADR-032), recall-calibration
(ADR-033), retrieval port (ADR-030), communication/schedule (ADR-040), and future
research packs will push the count well past ten. Each new pack would require editing the
dispatch crate, adding a Cargo dependency, and recompiling. The indirection bought nothing
— the MCP server already transitively depends on every pack crate it might load.

The architecture must satisfy:

1. **Zero-touch pack addition.** A new pack crate + its `inventory::submit!` line + a
   `KHIVE_PACKS` config entry. No edits to dispatch crates, no recompile of the kernel
   binary's coordinator.
2. **Dependency-aware loading.** `Pack::REQUIRES` (ADR-017) is enforced at startup: a pack
   that requires `kg` cannot load before `kg`; a cycle is a boot error.
3. **Compile-time module discovery.** Packs are statically linked, not dynamically loaded
   (no `dlopen`, no WASM). Discovery means "find all packs linked into this binary,"
   which `inventory` provides via linker-section registration.
4. **Discoverable.** `kkernel pack list` (ADR-003) enumerates available packs without
   reading docs or spawning a runtime.

## Decision

### `PackRegistry`: runtime pack discovery and loading

Replace the static dispatch crate with a `PackRegistry` in `khive-runtime`:

```rust
pub struct PackRegistry {
    factories: HashMap<&'static str, Box<dyn PackFactory>>,
}

pub trait PackFactory: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime>;
    fn requires(&self) -> &'static [&'static str];
}

impl PackRegistry {
    pub fn new() -> Self { Self { factories: HashMap::new() } }

    pub fn register<F: PackFactory + 'static>(&mut self, factory: F) {
        self.factories.insert(factory.name(), Box::new(factory));
    }

    /// Discover all packs linked into the binary via `inventory`.
    /// Called once at startup; collects every `PackFactory` registered
    /// via `inventory::submit!` across the link units.
    pub fn discover() -> Self {
        let mut registry = Self::new();
        for factory in inventory::iter::<Box<dyn PackFactory>> {
            registry.register(factory);
        }
        registry
    }

    /// Load packs by name, respecting dependency order via Pack::REQUIRES.
    /// Returns Err if a required pack is missing or if a cycle is detected.
    pub fn load(
        &self,
        names: &[&str],
        runtime: KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        let ordered = self.topo_sort(names)?;
        for name in ordered {
            let factory = self.factories.get(name)
                .ok_or_else(|| PackLoadError::NotFound(name.to_string()))?;
            builder.register(factory.create(runtime.clone()));
        }
        Ok(())
    }
}

pub enum PackLoadError {
    NotFound(String),
    MissingDependency { pack: String, requires: String },
    DependencyCycle(Vec<String>),
}
```

### Packs self-register via `inventory`

Each pack crate uses the `inventory` crate to declare its factory at link time:

```rust
// In khive-pack-kg/src/lib.rs:
pub struct KgPackFactory;

impl PackFactory for KgPackFactory {
    fn name(&self) -> &'static str { "kg" }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(KgPack::new(runtime))
    }
    fn requires(&self) -> &'static [&'static str] { &[] }
}

inventory::submit!(Box::new(KgPackFactory) as Box<dyn PackFactory>);
```

```rust
// In khive-pack-memory/src/lib.rs:
pub struct MemoryPackFactory;

impl PackFactory for MemoryPackFactory {
    fn name(&self) -> &'static str { "memory" }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(MemoryPack::new(runtime))
    }
    fn requires(&self) -> &'static [&'static str] { &["kg"] }
}

inventory::submit!(Box::new(MemoryPackFactory) as Box<dyn PackFactory>);
```

`inventory::submit!` uses linker-section registration (`.init_array` on ELF,
`__DATA,__mod_init_func` on Mach-O, equivalent on PE) — works on every tier-1 Rust target.
At startup, `inventory::iter::<Box<dyn PackFactory>>` collects every registered factory
across all link units.

### Pack selection and shipped production default

Pack selection uses two shipped sources in order of decreasing precedence:

1. **`--pack <name>` CLI flag** (repeatable): `khive-mcp --pack kg --pack gtd --pack memory`.
   Multiple `--pack` flags are joined into the final list. CLI flag wins over the runtime
   default.
2. **`KHIVE_PACKS` env var**: `KHIVE_PACKS=kg,gtd,memory`. Comma- or whitespace-separated
   pack names.
3. **Built-in production default**: when neither CLI nor `KHIVE_PACKS` provides a non-empty
   list, the shipped default is:

   ```text
   ["kg", "gtd", "memory", "brain", "comm", "schedule", "knowledge"]
   ```

Precedence: `--pack` CLI > `KHIVE_PACKS` env > production default.

The current `KhiveConfig` file parser does **not** parse a `packs = [...]` field. Config-file
pack selection and pack-scoped backend assignment remain deferred to the ADR-028/ADR-035
configuration work.

The MCP server passes the resolved list to `PackRegistry::register_packs()`, which:

1. Validates that every requested pack name is linked into the binary via `inventory`.
2. Requires all `PackFactory::requires()` dependencies to be explicitly present in the selected
   list; missing dependencies are boot errors, not auto-added.
3. Registers each selected pack, with `VerbRegistryBuilder::build()` enforcing the dependency
   load order.

### Delete `khive-dialect-kg`

The crate that held the static `match` is removed. The `DialectRegistrar` trait in
`khive-mcp` is removed. The `kkernel` and `khive-mcp` binaries use `PackRegistry::discover()`
directly.

Migration:

- Remove `khive-dialect-kg` from workspace `Cargo.toml`
- Remove `khive-mcp`'s dependency on `khive-dialect-kg`
- Add `khive-mcp`'s dependency on `inventory`
- Each pack crate gains `inventory::submit!`
- Kernel boot path uses `PackRegistry::discover()` instead of `KgDialect`

### Dependency ordering via `REQUIRES`

The `Pack::REQUIRES` const (ADR-017) drives topological sort. Examples:

```text
kg:     REQUIRES []           → loads first
gtd:    REQUIRES ["kg"]       → loads after kg
memory: REQUIRES ["kg"]       → loads after kg
brain:  REQUIRES ["memory"]   → loads after memory
schedule: REQUIRES ["kg"]     → loads after kg (parallel with gtd, memory)
```

`khive-fold` (ADR-024) and `khive-retrieval` (ADR-030) are foundation/runtime crates,
**not** packs. They are linked at compile time, not registered via `inventory`. Only
crates that implement `PackRuntime` (ADR-017) register as packs.

Circular dependencies are a load error, not a runtime error. The registry rejects the
cycle at startup with a clear message listing the offending pack names.

### Pack discovery for CLI

`kkernel pack list` (ADR-003) queries the registry:

```
$ kkernel pack list
kg          KG substrate (entities, edges, notes, search)
gtd         GTD task management (assign, next, complete)
memory      Memory with decay-aware recall (remember, recall)
brain       Event-driven auto-tuning (brain.state, brain.config)
schedule    Time-triggered actions (remind, schedule, agenda)
comm        Inter-agent messaging (send, inbox, read)
```

This is a side-effect of self-registration: every pack linked into the binary is
discoverable. The output reflects which packs are _available_, not which are _loaded_ —
`kkernel pack list --loaded` (or `--active`) shows the subset specified by `KHIVE_PACKS`.

### Pack-version reporting (optional, future)

`PackFactory` can expose a version string for introspection:

```rust
pub trait PackFactory: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn version(&self) -> &'static str { env!("CARGO_PKG_VERSION") }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime>;
    fn requires(&self) -> &'static [&'static str];
}
```

Default delegates to `CARGO_PKG_VERSION`. `kkernel pack list --verbose` includes the
version. Not blocking for initial dynamic-loading work; included here as a forward
compatibility hook.

## Rationale

### Why `inventory`, not feature flags

Feature flags would mean each new pack edits the kernel's `Cargo.toml` to add an opt-in
feature. That is exactly the maintenance burden this ADR removes. Feature flags also do
not solve the dependency-ordering problem — they let you exclude packs, but they do not
order the ones you include.

`inventory` is the right level of dynamism for compile-time modules. Packs that link in
get discovered; packs that do not link in are silently absent. `KHIVE_PACKS` then selects
the subset to actually load.

### Why not dlopen / WASM dynamic loading

Dynamic library loading (`dlopen`, WASM modules) would let packs ship as separate
binaries, hot-loadable at runtime. Pros: pack updates without kernel restart. Cons:
massive complexity (ABI stability across pack/kernel version skew, security boundaries,
WASM-host bridging for the Pack trait). khive is a single binary; packs are compile-time
modules. Dynamic linking solves a problem we do not have.

### Why deletion of `khive-dialect-kg`, not refactoring

The dialect crate's only function was the static `match`. Once `PackRegistry::discover()`
exists, the dialect crate has zero behavior. Keeping it as an empty re-export wrapper
adds compile-units for no benefit. Delete cleanly.

### Why `inventory` Box-of-trait-object pattern

`inventory::submit!(Box<dyn PackFactory>)` produces a `Box<dyn PackFactory>` per
registration; `inventory::iter` yields `&Box<dyn PackFactory>`. The Box is cheap (one
allocation per pack at link-time-equivalent initialization), and the trait-object
indirection is the standard cost of a registry of heterogeneous factories. Alternatives
(generic registration via type IDs, const-fn factories) either don't work with stable
Rust today or add complexity without benefit.

## Alternatives Considered

### A. Keep the static dialect; add more match arms

Pros: zero new infrastructure. Cons: every new pack is a code change in a crate that
exists only for indirection. At 10+ packs, the dispatch becomes a maintenance liability.
Rejected.

### B. Dynamic loading via dlopen / WASM

Pros: packs as separate binaries, hot-loadable. Cons: ABI stability, security boundaries,
debugging complexity. khive is a single binary. Rejected.

### C. Config-file-based registration (TOML maps pack name → crate)

Pros: no code change to add a pack. Cons: the TOML file still needs a match to map names
to constructors. Without `inventory`, someone has to write the dispatch. The config file
becomes another static list, just in TOML instead of Rust. Rejected.

### D. Feature-gated pack inclusion

Pros: minimal builds (kg-only) avoid linking unused pack code. Cons: every new pack edits
the kernel's `Cargo.toml`; feature combinations multiply in CI. Rejected as the primary
mechanism; can be layered on top of `inventory` if minimal builds become a hard
requirement (each pack feature-gates its `inventory::submit!`).

### E. Proc-macro for boilerplate reduction

`#[derive(Pack)]` or `pack!` macro generates the `PackFactory` impl + `inventory::submit!`
call. Pros: less boilerplate per pack. Cons: adds a proc-macro crate to maintain. Defer
until 10+ packs make boilerplate painful; the current ~10 LOC per pack is acceptable.

## Consequences

### Positive

- **Zero-touch pack addition** — new pack crate + `inventory::submit!` + `KHIVE_PACKS`
  config entry.
- **Dependency-aware loading** — `REQUIRES` enforced at startup; missing dependency or
  cycle is a clear boot error.
- **One fewer crate** — `khive-dialect-kg` removed from the dependency graph.
- **Discoverable** — `kkernel pack list` enumerates available packs without docs.

### Negative

- **`inventory` dependency** — adds ~200 LOC of well-maintained code; used by `tracing`
  and many other production crates. Uses linker-section registration; works on all tier-1
  Rust targets.
- **Pack factory boilerplate** — each pack gains ~10 LOC of `PackFactory` +
  `inventory::submit!`. Could be reduced with a proc macro (deferred).
- **Pack-load failures surface at boot** — a misconfigured `KHIVE_PACKS` (typo, missing
  dependency) fails fast. Mitigation: clear error messages naming the offending pack and
  dependency.

### Neutral

- **MCP wire format unchanged** — clients see the same verbs after they're registered.
- **Pack trait (ADR-017) unchanged** — this ADR adds a registry around the existing
  trait, not new trait methods.

## Open Questions

1. **Feature-gated packs**. Should some packs be behind cargo features (e.g.,
   `features = ["memory"]`)? This would allow minimal builds (kg-only) without linking
   memory/gtd/brain code. Trade-off: more feature-flag complexity in CI. Defer; the
   current "all packs linked, `KHIVE_PACKS` selects subset" model is simpler and the
   binary size impact is modest.
2. **Pack versioning at load time**. Should the registry validate that a pack's declared
   `REQUIRES` versions match installed pack versions (e.g., `requires = ["kg >= 0.2"]`)?
   v1: name-only matching, no semver. Add semver if version skew becomes a real problem.
3. **Pack config schemas**. Should each pack declare a config schema (e.g., `RecallConfig`
   for memory per ADR-033)? The registry could validate pack-specific config at load
   time. Defer to per-pack ADRs.

## Amendment 1 (2026-07-11): ten-pack default, and force-link is not zero-touch

**Status**: accepted

The "Pack selection and shipped production default" section's seven-pack list is stale.
The shipped `RuntimeConfig::default()` now loads ten production packs:

```text
["kg", "gtd", "memory", "brain", "comm", "schedule", "knowledge", "session", "git", "code"]
```

`session`, `git`, and `code` were added after this ADR was accepted (ADR-085 Amendment 3
most recently, adding `code`'s admin-CLI-only ingest path). Every surface that enumerates
the shipped default — this ADR, package READMEs, `docs/multi-backend.md` — must track the
`RuntimeConfig::default()` pack list and verb count, not a snapshot frozen at acceptance
time.

This ADR's "Zero-touch pack addition" claim (Context, item 1) and the "zero-touch"
framing implicit in `inventory::submit!` self-registration are correct only up to the
crate-linking boundary. `inventory`'s linker-section registration finds every pack that is
_linked into the binary_ — it does not cause a pack crate to be linked in the first place.
Each binary or server crate that wants a pack available at runtime still needs, at compile
time:

1. A `Cargo.toml` dependency on that pack crate.
2. A force-link anchor: at least one reference to a public symbol from the pack crate
   reachable from that binary's compiled code, so the linker does not discard the crate as
   unused. Rust does not otherwise guarantee an `inventory::submit!` static in a dependency
   survives dead-code elimination if nothing in the dependency graph visibly uses that
   crate.

`crates/khive-mcp/src/pack.rs` and `crates/kkernel/src/lib.rs` both carry an explicit
anchor block: one `#[doc(hidden)]` re-export or `use ... as _;` line per pack crate,
referencing a public type so the linker cannot discard it. Adding a pack to the shipped
default without adding it to both a `Cargo.toml` dependency list and one of these anchor
blocks means the pack is _never registered_ at runtime, regardless of what
`RuntimeConfig::default()` says — `khive-mcp`'s dependency on `khive-pack-code` plus its
`CodePack` anchor line, both added after the fact, are the concrete instance of this gap
(khive#848, finding 3).

`cargo metadata` reports exactly one workspace binary target (`kkernel`); `khive-mcp` is a
library consumed by that binary and by the `khive-mcp` server process it spawns, so today
there is exactly one anchor block that must stay in sync per binary, not N. That may not
remain true as more binaries are added — the ADR's "zero-touch" framing should be read as
"zero-touch once a pack crate is linked into every binary that needs it," not "adding a
pack crate to the workspace is sufficient on its own."

## Amendment 2 (2026-07-11): eleven-pack default, `workspace` added

**Status**: accepted

The `workspace` pack (#873) is now part of the shipped `RuntimeConfig::default()` set,
appended after `code`:

```text
["kg", "gtd", "memory", "brain", "comm", "schedule", "knowledge", "session", "git", "code", "workspace"]
```

Like `code`, `workspace` contributes zero verbs of its own: it registers the `workspace`
entity kind and five `contains` endpoint rules (workspace to issue/pull_request/commit/task/session
notes) and carries no MCP verbs. The default verb count is therefore unchanged at **78**;
only the pack count moves from ten to eleven. Every surface that enumerates the default set
(this ADR, package READMEs, `docs/guide/api-reference.md`, `docs/guide/specialized-packs.md`,
`docs/khive-config-example.toml`, `scripts/perf/mcp_bench_client.py`) tracks the eleven-pack
list. The force-link discipline of Amendment 1 applies: `workspace` carries a `Cargo.toml`
dependency and an anchor line in `crates/khive-mcp/src/pack.rs`.

## References

- [ADR-003](ADR-003-system-architecture.md) — `kkernel` binary; `pack list` introspection
- [ADR-017](ADR-017-pack-standard.md) — Pack trait, `REQUIRES` const, `PackRuntime`
- [ADR-023](ADR-023-declarative-pack-format.md) — third-party packs ship as Rust crates
  implementing the `Pack` trait (ADR-017) and self-register via `inventory::submit!`;
  the YAML-manifest model is rescinded
- [ADR-028](ADR-028-pack-scoped-backends.md) — extends pack configuration with per-pack
  backend assignment
- `inventory` crate — compile-time plugin registration via linker sections
