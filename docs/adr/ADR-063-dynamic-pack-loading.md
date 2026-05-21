# ADR-063: Dynamic Pack Loading — Replace Static Dialect with Registry

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Supersedes**: `khive-dialect-kg` crate (removed)\
**Depends on**: ADR-025 (Pack Standard), ADR-027 (Single-Tool MCP Surface)

## Context

`khive-dialect-kg` is a 50-LOC static registrar: a match statement mapping pack names to
constructors. It exists so `khive-mcp` doesn't import pack crates directly.

```rust
// This is the entirety of the dialect's logic:
match name {
    "kg"     => builder.register(KgPack::new(runtime)),
    "gtd"    => builder.register(GtdPack::new(runtime)),
    "memory" => builder.register(MemoryPack::new(runtime)),
    _        => Err(name),
}
```

This worked at 3 packs. It doesn't scale. The fold primitives (ADR-058), retrieval pipeline
(ADR-061), recall pipeline (ADR-062), and future packs (scheduling, CRM, areas, domains)
will push the count past 10. Each new pack requires editing `khive-dialect-kg`'s match arm,
adding a Cargo dependency, and recompiling the MCP server. The indirection buys nothing —
the MCP server already transitively depends on every pack crate.

The `Pack` trait (ADR-025) already defines everything needed for self-registration:
`NAME`, `VERBS`, `NOTE_KINDS`, `ENTITY_KINDS`, `REQUIRES`. Packs know their own identity.
The registry should discover packs, not hardcode them.

## Decision

### 1. `PackRegistry`: runtime pack discovery and loading

Replace the static dialect with a `PackRegistry` in `khive-runtime`:

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

    /// Load packs by name, respecting dependency order.
    /// Returns Err if a required pack is missing.
    pub fn load(
        &self,
        names: &[&str],
        runtime: KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        let ordered = self.topo_sort(names)?;
        for name in ordered {
            let factory = self.factories.get(name)
                .ok_or(PackLoadError::NotFound(name.to_string()))?;
            builder.register(factory.create(runtime.clone()));
        }
        Ok(())
    }
}
```

### 2. Packs self-register via `inventory`

Each pack crate uses the `inventory` crate to declare itself at link time:

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

inventory::submit!(KgPackFactory);
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

inventory::submit!(MemoryPackFactory);
```

At startup, the MCP server collects all registered factories:

```rust
let mut registry = PackRegistry::new();
for factory in inventory::iter::<Box<dyn PackFactory>> {
    registry.register(factory);
}
```

### 3. `KHIVE_PACKS` config selects which packs to load

Current behavior preserved: `KHIVE_PACKS=kg,gtd,memory` (or `RuntimeConfig.packs`).
The MCP server passes the list to `PackRegistry::load()`, which:

1. Resolves `requires` dependencies (transitive closure)
2. Topologically sorts (a pack loads after its dependencies)
3. Creates and registers each pack

If `KHIVE_PACKS` is empty or absent, default to `["kg"]` (minimal KG-only surface).

### 4. Delete `khive-dialect-kg`

The crate is removed. Its only function (static match dispatch) is replaced by
`PackRegistry::load()`. The `DialectRegistrar` trait in `khive-mcp/src/pack.rs` is removed.
`khive-mcp/src/server.rs` uses `PackRegistry` directly.

Migration:
- Remove `khive-dialect-kg` from workspace `Cargo.toml`
- Remove `khive-mcp`'s dependency on `khive-dialect-kg`
- Add `khive-mcp`'s dependency on `inventory`
- Each pack crate adds `inventory::submit!`
- `khive-mcp/src/server.rs` uses `PackRegistry` instead of `KgDialect`

### 5. Dependency ordering via `requires`

The `Pack` trait (ADR-025) already has `REQUIRES: &'static [&'static str]`. The pack registry
uses this for topological sort:

```
kg: requires []           → loads first
gtd: requires ["kg"]      → loads after kg
memory: requires ["kg"]   → loads after kg
brain: requires ["memory"] → loads after memory
```

Note: `khive-fold` (ADR-058) and retrieval objectives (ADR-061) are foundation/runtime crates,
not packs. They are linked at compile time, not loaded via the pack registry. Only crates that
implement `PackRuntime` (ADR-025) register as packs.

Circular dependencies are a load error, not a runtime error. The registry rejects the cycle
at startup with a clear message.

### 6. Pack discovery for CLI

The CLI (`khive --list-packs`) can query the registry for available packs:

```
$ khive --list-packs
kg       KG substrate (entities, edges, notes, search)
gtd      GTD task management (assign, next, complete)
memory   Memory with decay-aware recall (remember, recall)
brain    Event-driven auto-tuning (brain.state, brain.config)       [planned]
```

This is a side-effect of self-registration: packs that link into the binary are discoverable.

## Alternatives Considered

### A. Keep the static dialect, just add more match arms

Pros: zero new infrastructure. Cons: every new pack is a code change in a crate that exists
only for indirection. At 10+ packs, the match statement becomes a maintenance liability.
The dialect has no reason to exist if packs can self-register.

Rejected.

### B. Plugin system with dynamic loading (dlopen / wasm)

Pros: packs as separate binaries, hot-loadable. Cons: massive complexity increase (ABI
stability, version skew, security boundaries). khive is a single binary — packs are compile-time
modules, not runtime plugins. Dynamic linking solves a problem we don't have.

Rejected. `inventory` gives us compile-time discovery without runtime loading complexity.

### C. Config-file-based registration (TOML/YAML declares pack → crate mapping)

Pros: no code change to add a pack. Cons: the config file still needs a match to map names to
constructors. Without `inventory`, someone has to write the dispatch. The config file becomes
another static list, just in TOML instead of Rust.

Rejected. `inventory` is the right level of dynamism for compile-time modules.

## Consequences

### Positive

- **Zero-touch pack addition**: new pack crate + `inventory::submit!` + `KHIVE_PACKS` config
- **Dependency-aware loading**: `requires` fields enforced at startup, not hoped-for at runtime
- **One fewer crate**: `khive-dialect-kg` removed from the dependency graph
- **Discoverable**: `--list-packs` enumerates what's available without reading docs

### Negative

- **`inventory` dependency**: adds one crate (~200 LOC, well-maintained, used by tracing et al).
  Uses linker section registration (`.init_array` on ELF, `__DATA,__mod_init_func` on Mach-O) —
  works on all tier-1 Rust targets.
- **Pack factory boilerplate**: each pack gains ~10 LOC of `PackFactory` + `inventory::submit!`.
  Could be reduced with a proc macro if it becomes tedious at 10+ packs.

## Open Questions

1. **Feature-gated packs**: should some packs be behind cargo features (e.g., `features = ["memory"]`)?
   This would allow minimal builds (kg-only) without linking memory/gtd code. Trade-off: more
   feature-flag complexity in CI.
2. **Pack versioning**: when packs evolve independently, should `PackFactory` report a version?
   `VerbDef` currently has no version field. Future concern — not blocking for initial migration.
3. **Pack config**: should each pack declare a config schema (e.g., `RecallConfig` for memory)?
   The registry could validate pack-specific config at load time.

## References

- ADR-025: Pack Standard — `Pack` trait, `VERBS`, `NOTE_KINDS`, `REQUIRES`
- ADR-027: Single-Tool MCP Surface — `request` verb dispatches to pack verbs
- `khive-dialect-kg/src/lib.rs` — the 50-LOC static registrar being replaced
- `khive-mcp/src/pack.rs` — `DialectRegistrar` trait (removed by this ADR)
- `inventory` crate: compile-time plugin registration via linker sections
