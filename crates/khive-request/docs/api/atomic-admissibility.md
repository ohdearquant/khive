# Atomic Admissibility

ADR-099 requires a would-be atomic operation file to reject every inadmissible verb before any execution. `check_atomic_admissible` is the pure preflight seam that applies the shared pack-metadata policy to already parsed operations.

## `AtomicRejection`

Each rejection records the zero-based operation index, exact tool name, and `AtomicRejectionReason`. Its `Display` output names the offending operation, explains the category, and lists the currently admissible verb set.

Reasons distinguish embedding-bearing writes, reads with no write plan, unlisted verbs, and verbs that are conceptually admissible but whose prepare/apply seam is not implemented. `merge` receives a specific actionable message directing callers to the non-atomic merge verb because full field folding, index cleanup, and edge-conflict handling are deferred.

## `check_atomic_admissible`

The function visits every input operation in order and returns every rejection, not just the first. An empty vector means all tool names pass the current static policy.

The check delegates to `khive_types::pack::atomic_admissibility`, so the admissible set remains shared pack metadata rather than a second parser-owned list. It examines only tool names, performs no I/O, constructs no runtime, mutates nothing, and never executes an operation. A future atomic runner must call it before beginning its prepare pass.

Atomic admissibility is separate from DSL validity, tool registration, argument validation, and write-key conflict detection. Passing this check promises only that the tool name is eligible for the current atomic execution design.
