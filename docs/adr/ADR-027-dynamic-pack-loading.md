# ADR-027: Dynamic Pack Loading via Self-Registration

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

ADR-017 establishes packs as composable vocabulary + verb-handler bundles, declaring
`NAME`, `VERBS`, `NOTE_KINDS`, `ENTITY_KINDS`, and `REQUIRES`. The boot path must
construct the right packs based on configuration, respecting `REQUIRES` dependencies.

The historical implementation used a static dispatch crate with one match arm per
pack. That design does not scale: each new pack requires editing a central dispatch
crate even though the binary already links every pack it can load.

The architecture must satisfy:

1. **Localized pack addition.** A new linked pack crate registers its factory with
   `inventory::submit!`; no central dispatch match must be edited.
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

`inventory::submit!` uses linker-section registration (`.init_array` on ELF,
`__DATA,__mod_init_func` on Mach-O, equivalent on PE): works on every tier-1 Rust target.
At startup, `inventory::iter::<Box<dyn PackFactory>>` collects every registered factory
across all link units.

### Pack selection

The runtime resolves a configured list of pack names and passes it to
`PackRegistry::register_packs()`, which:

1. Validates that every requested pack is linked into the binary through `inventory`.
2. Requires dependencies declared by `PackFactory::requires()` to be present.
3. Registers the selected packs in topological order.

A deployment that selects only `kg` loads only the public KG pack.

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

The `Pack::REQUIRES` const (ADR-017) drives topological sort. The kg pack declares
no dependencies and therefore loads first when selected. Foundation and runtime crates
are not packs; only crates implementing `PackRuntime` register through `inventory`.

Circular dependencies are a load error, not a runtime error. The registry rejects the
cycle at startup with a clear message listing the offending pack names.

### Pack discovery for CLI

`kkernel pack list` (ADR-003) queries the registry:

```text
$ kkernel pack list
kg          KG substrate (entities, edges, notes, search)
```

Self-registration makes every linked pack discoverable. The output reflects which
packs are available; `kkernel pack list --loaded` shows the selected subset.

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
not solve the dependency-ordering problem: they let you exclude packs, but they do not
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

Pros: zero new infrastructure. Cons: every extension requires a code change in a crate
that exists only for indirection, so discovery and dispatch remain centrally coupled.
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
until registration boilerplate becomes a demonstrated maintenance problem.

## Consequences

### Positive

- **Zero-touch pack addition**: new pack crate + `inventory::submit!` + `KHIVE_PACKS`
  config entry.
- **Dependency-aware loading**: `REQUIRES` enforced at startup; missing dependency or
  cycle is a clear boot error.
- **One fewer crate**: `khive-dialect-kg` removed from the dependency graph.
- **Discoverable**: `kkernel pack list` enumerates available packs without docs.

### Negative

- **`inventory` dependency**: adds ~200 LOC of well-maintained code; used by `tracing`
  and many other production crates. Uses linker-section registration; works on all tier-1
  Rust targets.
- **Pack factory boilerplate**: each pack gains ~10 LOC of `PackFactory` +
  `inventory::submit!`. Could be reduced with a proc macro (deferred).
- **Pack-load failures surface at boot**: a misconfigured `KHIVE_PACKS` (typo, missing
  dependency) fails fast. Mitigation: clear error messages naming the offending pack and
  dependency.

### Neutral

- **MCP wire format unchanged**: clients see the same verbs after they're registered.
- **Pack trait (ADR-017) unchanged**: this ADR adds a registry around the existing
  trait, not new trait methods.

## Open Questions

1. **Pack versioning at load time.** Should the registry validate dependency version
   requirements rather than matching names only? v1 uses name-only matching; semantic
   version constraints can be added if version skew becomes a concrete problem.

## References

- [ADR-003](./ADR-003-system-architecture.md): `kkernel` binary; `pack list` introspection
- [ADR-017](./ADR-017-pack-standard.md): Pack trait, `REQUIRES` const, `PackRuntime`
- [ADR-023](./ADR-023-declarative-pack-format.md): third-party packs ship as Rust crates
  implementing the `Pack` trait (ADR-017) and self-register via `inventory::submit!`;
  the YAML-manifest model is rescinded
- [ADR-028](./ADR-028-pack-scoped-backends.md): extends pack configuration with per-pack
  backend assignment
- `inventory` crate: compile-time plugin registration via linker sections
