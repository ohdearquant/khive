# Runtime lifetime and store release

The owner inventory below was traced at `cafa571463d70ce8ae4c25ff47b782807b1193eb`.
**Drop is best effort.** It does not prove that dropping a runtime settles its storage. An embedding host that evicts tenant runtimes needs
both task completion and closure of the tenant's file handles before eviction is
complete.

`KhiveRuntime` is cloneable and has no custom destructor. A last _host_ handle is
not necessarily the last owner: registries, stores, in-flight requests, background
work, and process caches can retain a backend. User-provided packs, embedders,
blob stores, hooks, and fusion executors are extension boundaries; their private
resources cannot be discovered or joined through the current runtime interface.

## Construction and ownership map

All locations below refer to the source snapshot above. "Later" means that
ordinary `drop` does not wait for the described completion.

| Resource / activation                                                                                            | Owner and stop condition                                                                                                                                                                                                                                                                            | Completion relative to runtime drop                                                                                                        | Source                                                                                                                                                                            |
| ---------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Main and optional core backends; named-vector stores; diagnostic pools                                           | Runtime fields and all cloned storage/registry handles hold backend or pool `Arc`s. Late diagnostic entries are weak. No runtime destructor coordinates them.                                                                                                                                       | Final-owner field destruction is synchronous; host-handle drop alone does not establish final ownership.                                   | `khive-runtime/src/runtime.rs:327`, `:709`, `:774`, `:994`                                                                                                                        |
| Pool reader connections and pool writer; SQLite DB/WAL/SHM handles                                               | `ConnectionPool::drop` drains idle readers before field destruction closes its writer. Checked-out readers retain their owner.                                                                                                                                                                      | Synchronous when the pool actually drops, independently of the standalone writer below.                                                    | `khive-db/src/pool.rs:383`                                                                                                                                                        |
| Standalone writer task and its independent SQLite connection, started lazily by write routing                    | Pool owns the channel sender and an optional join. All sender clones must disappear; the receive loop drains then exits. Dropping its join detaches it. The task does not need a pool `Arc` to keep its connection alive.                                                                           | Later. `take_writer_task_join` permits a caller to wait; a dead pool `Weak` alone is insufficient.                                         | `khive-db/src/pool/pool_impl.rs:454`, `:617`; `khive-db/src/writer_task.rs:1196`, `:1399`                                                                                         |
| Writer request blocking work and connection settlement                                                           | The writer moves its connection into `spawn_blocking`, awaits the request, then resumes or retires it. Cancelling an async parent does not stop already-started blocking work.                                                                                                                      | Later; normal writer join establishes the drain boundary.                                                                                  | `khive-db/src/writer_task.rs:1399`                                                                                                                                                |
| Audit supervisor, generation children, and abandoned-append reapers, after audited dispatch                      | `VerbRegistry` owns `AuditBatch`; supervisors and children clone the event store. `shutdown_audit_batch` calls `close_and_drain`. There is no audit owner field on `KhiveRuntime`. The explicit close barrier additionally tracks physical supervisor, child and reaper completion.                 | Runtime drop does not drain them. Ordinary supervisor join is explicit; an abandoned downstream append needs separate settlement evidence. | `khive-runtime/src/pack/builder.rs:572`; `pack/registry_access.rs:370`; `audit_batch.rs:708`, `:922`, `:976`, `:1102`                                                             |
| Embedded events-split backend, opened on first event access                                                      | A process-global strong `BackendMap` retains the backend by canonical path. The cleanup guard is test-only.                                                                                                                                                                                         | Runtime drop does not release the cache entry or its connections.                                                                          | `khive-runtime/src/runtime.rs:1327`; `events_split/registry.rs:26`, `:35`, `:86`                                                                                                  |
| Socket events client, preflight in-memory store, queue, and forwarder                                            | A process-global strong `ClientMap` retains the sender/client. Forwarder is tracked process-wide and observes daemon shutdown; request sockets live with their calls.                                                                                                                               | Runtime drop neither closes the cached sender nor cancels the process-wide token.                                                          | `khive-runtime/src/events_split/registry.rs:24`, `:102`; `events_split/forwarder.rs:169`, `:237`, `:252`                                                                          |
| Optional embedder initialization task and blocking embedding worker thread, after first use                      | Initialization clones the runtime and is awaited by its caller, but caller cancellation detaches it. Worker thread owns the model service and a receiver; the sender is in the embedding service, and the thread join is discarded.                                                                 | Later; worker drains until all senders disappear. Provider-owned model files require provider lifetime accounting.                         | `khive-runtime/src/runtime/embedder_init.rs:30`, `:68`; `embedder_registry.rs:220`                                                                                                |
| Memory ANN mmaps, rebuild work, rotation watcher and dirty checkpoint timer, after ANN use                       | Pack ANN state owns index bridges. Rotation uses a weak reference between ticks and stops on a later tick or global shutdown. Rebuilds and timers can retain runtime clones; blocking builds and index files belong to those owners.                                                                | Later, with no tenant-scoped join surface. A global daemon cancellation cannot safely evict just one tenant.                               | `khive-pack-memory/src/ann/state.rs:26`, `:90`; `ann/warm.rs:56`, `:170`, `:888`; `ann/checkpoint_timer.rs:81`; `khive-retrieval/src/ann.rs:112`; `khive-vamana/src/bridge.rs:19` |
| Configured blob root directory handle and in-flight blob work                                                    | `FsBlobStore` owns an `Arc<File>`; runtime hydrator owns the store. Blocking calls clone the root handle. Hydration supervisor is process-tracked and outlives a cancelled request.                                                                                                                 | Last idle store handle closes synchronously; admitted blocking/hydration work may finish later.                                            | `khive-db/src/stores/blob.rs:166`, `:319`, `:414`; `khive-runtime/src/blob.rs:27`, `:235`                                                                                         |
| Code-map descriptors                                                                                             | Registry retains validated path/identity metadata, not an open database. Each map read opens its own read-only connection and explicitly drops it after its transaction.                                                                                                                            | Synchronous after each completed read; a live caller still owns its own connection.                                                        | `khive-runtime/src/code_map_runtime.rs:95`, `:251`, `:274`                                                                                                                        |
| Boot schema/cutover blocking tasks                                                                               | Async host builders await schema preparation and cutover before returning the prepared runtime. Constructor errors and cancellation are distinct from successful boot.                                                                                                                              | Successful return has awaited this work; cancellation must not be confused with that boundary.                                             | `khive-mcp/src/serve/runtime_config.rs:33`, `:106`                                                                                                                                |
| Host checkpoint loop, dedicated checkpoint SQLite connection, checkpoint blocking cycle, lifecycle append worker | Started by daemon hosting, not by `KhiveRuntime::new`. Loop holds a pool; watch-channel shutdown is explicit. Checkpoint cycle owns its connection off-worker. Lifecycle shutdown aborts and joins its own async worker but explicitly allows one admitted downstream sink operation to outlive it. | Later; joining the checkpoint loop alone does not settle every downstream write.                                                           | `khive-runtime/src/daemon/boot_guard.rs:287`; `khive-db/src/checkpoint/task_loop.rs:45`; `checkpoint/off_worker.rs:39`; `checkpoint/lifecycle.rs:294`                             |
| Session WAL-pin sweep and sidecar files                                                                          | Serve host owns watch sender and join, with a two-second bounded shutdown. Sidecar state writes transient file handles; heartbeat removal is shutdown work. Remaining beacon files do not imply open handles.                                                                                       | Explicit host shutdown waits; dropping a runtime does not stop its host's sweep.                                                           | `khive-mcp/src/serve/startup.rs:360`, `:410`; `khive-db/src/checkpoint/walpin_sidecar.rs:328`, `:498`                                                                             |
| Per-operation volume lock file and process slot                                                                  | Admitted write units own a `VolumeLease`; its destructor drops the lock file before the in-process slot. Files can remain on disk while unlocked.                                                                                                                                                   | Synchronous after actual native settlement; a request reply alone is not a generic lifetime witness.                                       | `khive-db/src/disk_guard.rs:549`, `:594`; `write_units.rs:405`; `writer_task.rs:1332`                                                                                             |
| Optional checkpoint FTS maintenance child                                                                        | A due main-checkpoint tick starts blocking work holding the pool and an admitted standalone connection. Secondary checkpoints disable this maintenance.                                                                                                                                             | Graceful checkpoint join awaits it; cancelling the parent cannot stop already-started blocking work.                                       | `khive-db/src/checkpoint/task_loop.rs:33`, `:190`, `:254`; `pool/pool_impl.rs:759`                                                                                                |
| Timeout diagnostics thread and NDJSON log                                                                        | Writable pools register per resolved log directory. A retained thread join is shared by close handles; pool and native-writer leases protect live diagnostics. The default log is outside the store; configured placement can put it inside.                                                        | Ordinary Drop keeps the sink alive; explicit close drains and joins after the last pool and native writer release it.                      | `khive-db/src/pool/construction.rs:321`; `khive-db/src/timeout_sink.rs:384`, `:814`, `:907`, `:999`                                                                               |
| Daemon listener, connection tasks, warm-up and optional channel loops                                            | Host startup owns these resources and uses process-wide shutdown/tracking. Building a runtime or server does not itself enter the serve loop.                                                                                                                                                       | Host lifecycle, not the bare runtime lifecycle.                                                                                            | `khive-runtime/src/daemon/boot_guard.rs:78`, `:259`; `daemon/dispatch_trait.rs:359`, `:412`; `khive-mcp/src/serve/startup.rs:448`                                                 |

## Existing explicit settle sequence

`kkernel/src/exec/run.rs:570` documents the current exec sequence:

1. Drain the registry audit batch.
2. Take every pool's standalone writer join.
3. Drop the server and all local storage references.
4. Await those joins, bounded at five seconds each.

It logs and swallows failures, so it is advisory rather than a successful-return
close guarantee. It also does not evict events process caches or expose a
per-tenant boundary for the conditional resources above. A shared close helper
must preserve this existing exec behavior and distinguish tenant-owned resources
from process-owned services before claiming that it releases every resource.

## Explicit close for a quiescent host slot

`khive_runtime::runtime::RuntimeCloseOwner` owns the slot's matching registry and
all runtime clones transferred by its host. The host stops new requests and waits
for its active-request counter before constructing this bundle. It declares any
host checkpoint or MCP sweep tasks in `RuntimeCloseHostTasks`; these tasks cannot
be stopped by a tenant runtime.

```rust,ignore
let mut owner = RuntimeCloseOwner::new(
    registry,
    vec![runtime, entry_counter_runtime_clone],
    RuntimeCloseHostTasks::default(), // neither host background task exists
);
owner.close().await?;
```

There are two explicit phases. `Ready` performs reversible preflight: a
`Refused(owner)` result leaves the slot's registry, writer and audit admission
fully usable through `open_registry()` and `open_runtimes()`. Once preflight
passes, `Closing` commits teardown; cancelling its future does not restore
admission. `Incomplete` belongs to this committed phase and is resumable, not a
reversible refusal. After `Incomplete`, **do not reopen that root until close
returns `Ok`**, because the first writer may still be live. The host holds this
boundary under its per-tenant lock. Successful release advances to `Closed`.

The supported release path stops audit admission and waits for physical
supervisor, append and abandoned-reaper completion; retains each writer join;
drops the transferred registry/runtime roots; joins the writer; and closes and
joins the timeout sink after its last store and writer release it. The sink is
shared by its resolved log directory, rather than selected by the first process
pool. Ordinary pool Drop keeps the sink alive as before. Closing one slot cannot
stop diagnostics for another live store sharing that directory.

Cancellation of `close` retains its progress and joins on the owner. Keep the
owner and retry. A second call after success returns `Ok(())`. A caller timeout
is not successful release and never aborts an admitted native write. If an
outside holder retains a pool, backend or audit store, committed close returns
`Incomplete { roots }` with the surviving weak-root kinds and strong counts;
release that holder and retry. The same result names live shared-sink pool/writer
counts until those owners finish. The counts describe observed roots, not every
erased capability holder: a separately retained note capability, for example,
keeps the observed pool alive without exposing its own type identity. A join taken by another caller returns
`WriterJoinUnavailable`, even if that caller later finishes it: absence of a
handle is not this owner's evidence of completion. Audit terminal errors remain
errors after physical cleanup; they are never relabeled successful writes.

The registry, packs and event store must belong to the supplied slot. Custom
packs must not hide independent file/task owners. This is a runtime-storage
release API, not a process-wide census or a shutdown contract for arbitrary
providers. `writer_tasks_finished()` reports the retained task completion
witness; `is_closed()` requires successful completion of the entire sequence.

### Explicitly unsupported owners

Close refuses these configurations before consuming the slot or taking joins:

- Split/socket events: process-global backend/client registries retain ownership.
- Daemon checkpoint loop and MCP session sweep: their host owns shutdown and joins.
  A claimed pool checkpoint owner is also detected independently of the host flags.
- Embedding providers: cancelled initialization can detach a runtime-owning task;
  the native embedding worker's thread join is not retained. Registered providers
  are conservatively refused even if a particular model has not yet loaded.
- Pack ANN hooks/providers: rebuilds, rotation watchers and checkpoint timers lack
  one tenant-scoped physical-completion owner. The mapped index can outlive its
  original file descriptor, so an empty FD census cannot prove mmap release.
- Blob hydration: admitted supervisors and blocking work need their own physical
  completion barrier. An installed hydrator is conservatively refused.
- Secondary/multiple backend composition: this first owner covers one pool and
  its clones. Additional diagnostic backends are refused rather than omitted.

Embedding-enabled tenant eviction therefore remains unsupported in this slice;
turning embeddings off in a fixture does not prove the production embedding/ANN
configuration. Default construction alone is not evidence that those owners have
already completed.

## Release measurements and controls

`khive-mcp/tests/runtime_close.rs` is an ordinary test over a private file-backed
runtime, KG registry, entity/note writes and a real persisted audit. It keeps the
three `runtime_close_measurement` lines, detects a live canary, and checks the
writer completion witness, expired pool weak reference, and empty handle census
immediately after close. A reopened runtime commits its first write only after
that evidence; successful reopening alone cannot establish single-writer
ownership because SQLite permits concurrent writable connections.

The cancellation regression holds a real admitted native writer closure while
cancelling its request and twice timing out close. Only releasing that native
closure allows successful close, and its committed row remains readable. This
falsifies a close implementation that skips the writer join. The observed live
sink descriptor before close supplies the no-sink-close control. Shared sink
coverage keeps another store alive and requires its diagnostics to remain usable.

The process census adapters cover Linux (`/proc/self/fd`) and macOS (libproc plus
`F_GETPATH`); unsupported platforms return an error, never an empty census. The
adapter itself has live file and directory canaries. This does not yet establish
Windows handle release, provider resources, external aliases, or mmap release.
