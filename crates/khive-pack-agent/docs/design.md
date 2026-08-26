# khive-pack-agent Design

## Purpose

`khive-pack-agent` owns the agent-facing wire surface defined by ADR-142: `agent.spawn`,
`agent.observe`, `agent.suspend`, `agent.resume`, and `agent.kill`. The runtime owns agent process
records and their durable table; this pack validates parameters and delegates every read and write
through `khive_storage::AgentStore`.

## Key types and modules

- `AgentPack` holds the injected `Arc<dyn AgentStore>` and implements both the static `Pack`
  contract and runtime dispatch.
- `pack.rs` defines the five visible handler descriptions and maps verb names to handlers.
- `handlers.rs` validates wire parameters, computes spawn fingerprints, serializes records, and
  applies the shared runtime lifecycle transition table.
- `vocab.rs` intentionally contributes no note or entity kinds: an agent is a runtime-owned
  process record, not a graph substrate record.
- `AgentRecord`, `AgentState`, `TerminalReason`, and transition helpers come from the shared type
  and runtime crates so storage and the verb surface use the same state model.

## Invariants

- The pack never opens a khive database connection. All process-table access goes through the
  injected `AgentStore` trait object.
- `AgentPack` is registered manually with `RegistryBuilder::register`, not through inventory.
  Inventory factories receive only `KhiveRuntime`, which does not expose the required store under
  this contract.
- `agent.spawn` requires non-empty `provider` and `task`. Its fingerprint covers the semantic
  spawn inputs, while a caller idempotency key is scoped to the owning actor. Reusing a key with
  different inputs fails instead of silently returning the earlier agent.
- At most one non-terminal record may bind a `(provider, provider_session_id)` pair.
- Spawn captures the caller's actor and namespace context in the process record. Native in-process
  dispatch is marked with the distinguished `native` peer class.
- Lifecycle changes use the shared transition table. Suspend is legal from running, resume from
  suspended, and kill from any non-terminal state. Already-suspended, already-running, and
  already-terminal kill requests are idempotent no-ops where defined by that table.
- `agent.suspend` does not create a session checkpoint. It reports the checkpoint id already stored
  on the process record; checkpoint content is outside this verb's input surface.
