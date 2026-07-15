# Pack Integration and Tuning

`MemoryPack` connects the memory verbs to the runtime, declares schema and dependencies, installs mutation hooks, warms indexes, and exposes recall weights to the brain tuning interface.

## `MemoryPack::new`

`MemoryPack::new(runtime)` creates shared ANN state, a default-capacity query embedding cache, default recall configuration, and a `BalancedRecallState` with entity-posterior capacity 10,000. The pack retains the runtime used to construct it; multi-backend hooks must attach to that runtime rather than an unrelated registration argument.

## Pack contract

The pack name is `memory`, its note kind is `memory`, and it requires `kg`. Inventory registration makes the factory discoverable without a call-site list. `MemoryPack::SCHEMA_PLAN` owns the durable ANN epoch table in accordance with the pack schema contract.

Verb categories are intentional: recall and its dotted stages are assertive; remember, feedback, prune, and vacuum mutate state and use the corresponding commissive/declaration classifications defined in the handler table.

## Warm phase

`PackRuntime::warm` schedules ANN warming for registered embedding models, then runs an FTS population guard. The guard compares live base rows with unified FTS rows and warns when a database with more than 100 rows has less than half represented in FTS. It never hard-fails boot and skips legitimately new or empty databases. This detects the V3-to-V4 migration failure mode where empty unified tables stranded recall until manual reindexing.

## Note-mutation hook

Generic KG update, delete, and merge paths do not depend on the memory crate. `register_note_mutation_hook` bridges those paths: when the changed note kind is `memory`, it bumps every registered model generation and schedules background warming.

The hook ignores other note kinds and authorization failure. It attaches to `self.runtime`, matching the per-pack runtime in multi-backend deployments. It no longer clears graphs or snapshots; readers may use the intact stale graph while a newer generation builds.

The hook must be installed before expecting generic KG mutations to invalidate memory ANN state. Production registry boot calls the pack hook registration seam; hand-built tests must call it explicitly.

## Dispatch and deadline boundary

`dispatch` routes the public memory verbs and the dotted subhandlers. Unknown names return `InvalidInput`. The `memory.recall` route goes through the end-to-end deadline wrapper before entering the main handler. Other verbs call their concern-specific handlers directly.

`registered_embedding_model_names` reports the model count used by dispatch audit resource accounting when remember has no explicit model override.

## `PackTunable`

The memory pack exposes three parameters:

- `memory::relevance_weight`
- `memory::salience_weight`
- `memory::temporal_weight`

`current_state` returns the pack's `BalancedRecallState`. `project_config` maps posterior means into a `RecallConfig`, preserving unrelated configuration. `apply_config` validates a projected configuration before replacing the active value, so tuning cannot install a negative or all-zero weight set.

The three posterior domains correspond directly to the three recall weights. Serve-time projection for a named brain profile is request-local; applying a pack tuning artifact changes the pack's active default configuration.

## Configuration concurrency

Active configuration is protected by a mutex. Readers lock and clone the complete configuration, preventing a request from observing a partially applied update. Validated replacements swap the stored configuration atomically under the same mutex.
