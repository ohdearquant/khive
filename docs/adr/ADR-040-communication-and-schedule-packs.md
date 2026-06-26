# ADR-040: Communication and Schedule Packs

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive

## Context

The pack standard (ADR-017) specifies how vocabulary, verb handlers, kind specialization, and
edge endpoint rules compose into a runtime. khive ships three first-party packs as canonical
references: `kg` (knowledge graph vocabulary and CRUD), `gtd` (task lifecycle — ADR-019), and
`memory` (decay-weighted recall — ADR-021).

Two domains remain unaddressed by the current pack set:

1. **Communication** — agents need to send messages, track conversations, and coordinate with
   other agents or humans across sessions. Today this happens outside khive via MCP tools or
   direct API calls, forfeiting the structured persistence that packs provide: messages are
   not retrievable via `recall`, not linkable to KG concepts, not namespaced under the same
   authorization gate (ADR-018).

2. **Schedule** — time-triggered actions (reminders, recurring tasks, deadlines) have no
   native pack representation. GTD tracks _what_ needs doing, not _when_. An agent wanting
   "remind me in two hours" or "check this daily" has no pack-level intent primitive. Intent
   must be stored somewhere before an execution mechanism can act on it.

Both domains appeared in the original internal implementation and were excluded from the v0.1
release pending design settlement. This ADR specifies them as two new first-party Rust packs:
`khive-pack-comm` and `khive-pack-schedule`.

The system must satisfy:

1. **No substrate fork.** Both new note kinds (`message`, `scheduled_event`) ride the existing
   notes table. No new storage trait, no additional migration, no parallel CRUD path.
2. **Disjoint verbs.** No collision with kg, gtd, or memory verb names.
3. **Event observable disambiguation.** The substrate's `Event` type (ADR-004) is a read-only
   audit observable. The schedule pack's `scheduled_event` note kind is user-authored future
   intent. These must not be conflated.
4. **Mailbox model for comm.** No real-time delivery mechanism. Agents poll via `inbox`.
   This matches the agent-scale interaction model and avoids network/pubsub dependencies.
5. **Intent storage for schedule.** The pack stores what should happen and when. Trigger
   evaluation (replay the stored verb+args payload at the designated time) is the runtime's
   and execution environment's responsibility — the pack does not own a polling loop.

## Decision

### Part 1: Communication pack (`khive-pack-comm`)

#### Pack identity

```rust
// crates/khive-pack-comm/src/lib.rs
pub struct CommPack { ... }

impl Pack for CommPack {
    const NAME:         &'static str            = "comm";
    const NOTE_KINDS:   &'static [&'static str] = &["message"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS:     &'static [HandlerDef]   = &[
        HandlerDef { name: "comm.send",   description: "Send a message, optionally threaded.",                             visibility: Visibility::Verb },
        HandlerDef { name: "comm.inbox",  description: "List inbound messages for the caller.",                            visibility: Visibility::Verb },
        HandlerDef { name: "comm.read",   description: "Mark an inbound message as read.",                                 visibility: Visibility::Verb },
        HandlerDef { name: "comm.reply",  description: "Reply to a message, threading linkage.",                           visibility: Visibility::Verb },
        HandlerDef { name: "comm.thread", description: "Retrieve all messages in a conversation thread, chronologically.", visibility: Visibility::Verb },
    ];
    // ADR-023 §4: pack-prefixed verb names — `comm.send` / `comm.inbox` / `comm.read` / `comm.reply` / `comm.thread`
    const EDGE_RULES:   &'static [EdgeEndpointRule] = &[];
    const REQUIRES:     &'static [&'static str] = &["kg"];
}
```

#### Notes-as-messages

A `message` is a note. `kind = "message"` is registered with the runtime via `NOTE_KINDS`.
The `properties` JSON column carries message-specific metadata:

```json
{
  "from": "agent:ocean",
  "to": "agent:khive",
  "direction": "inbound",
  "subject": "retrieval port status",
  "thread_id": "a1b2c3d4-...",
  "read": false,
  "sent_at": "2026-05-23T10:00:00Z"
}
```

The `content` field on the note is the message body. Subject is optional metadata in
`properties`; all message-specific fields live in `properties`, not as separate columns.

`direction` is stored from the recipient's perspective: `inbound` (message received by the
caller's namespace) or `outbound` (message sent by the caller). This is set by `send` and
`reply` at write time — callers do not supply it.

#### Five verbs

| Verb          | Speech act (ADR-025) | Args                                      | What it does                                                                                                                                                                                                                                                                              |
| ------------- | -------------------- | ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `comm.send`   | commissive           | `to`, `subject?`, `content`, `thread_id?` | Create a message note in the recipient's namespace (`direction=inbound`) and an outbound copy in the caller's namespace (`direction=outbound`). `from` is set to the caller's identity. Both writes are atomic: if the inbound write fails, the outbound copy is rolled back.             |
| `comm.inbox`  | assertive            | `limit?`, `status?`                       | List inbound messages (`direction=inbound`) for the caller. `status` filters on `read`: `unread` (default), `read`, or `all`. Uses a paginated scan so that inbound messages are never missed behind a deep outbound backlog.                                                             |
| `comm.read`   | declaration          | `id`                                      | Set `properties.read = true` on an **inbound** message. Returns the updated message envelope. Outbound messages cannot be marked read; the verb returns an error if `direction=outbound`.                                                                                                 |
| `comm.reply`  | commissive           | `id`, `content`                           | Fetch the target message's `thread_id` (or use the message's own UUID as the thread root). Create a new message with the same `thread_id`, `to` set to the other party, `subject` prefixed with `"Re: "` if not already. Uses dual-write for inbound delivery to the recipient.           |
| `comm.thread` | assertive            | `id`, `limit?`                            | Validate the root message by UUID (must exist, must be `kind=message`), then return the root plus all messages whose `properties.thread_id` equals the root UUID, sorted by `created_at` ascending (chronological). Uses a paginated scan. `id` accepts 8-char short prefix or full UUID. |

#### Message-filter scan cap

`list(kind=message, direction=…)` and similar filtered calls route through the KG pack's
paginated scan path. The scan reads the note store in 200-row pages (newest-first) and applies
in-memory filters until `limit` matches are collected. To bound worst-case cost on very large
stores (e.g. 1 M+ messages), the scan stops after at most **10 000 unfiltered rows**
(`MAX_SCAN_TOTAL` in `khive-pack-kg/src/handlers.rs`).

Callers with deep mailboxes should prefer the dedicated comm verbs, which are not subject to
this cap:

- `comm.inbox` — paginates through the store by namespace until `limit` inbound rows are found;
  no total-scan ceiling.
- `comm.thread` — indexed by `thread_id`; scans the full store but exits early once every page
  returns no new matches.

The 10 000-row cap is an implementation detail and may be raised or made configurable in a
future release.

#### Threading model

Threading is flat. A `thread_id` is the UUID of the root message in a conversation. All
replies carry the same `thread_id`. The pack does not enforce tree structure — callers can
reconstruct conversation order from `sent_at` on messages sharing a `thread_id`.

`comm.reply(id)` resolves the thread root: if the target message has a `thread_id`, that value
is propagated; otherwise the target message's own UUID becomes the `thread_id` for the new
message chain.

#### Cross-namespace messaging (deferred Option B — multi-actor path)

**Note (2026-06-17, ADR-007 Rev 3)**: The cross-namespace allowlist model described in this
section is the deferred Option B (multi-actor deployment path) from ADR-057. It is NOT the
current default implementation. Under ADR-007 Rev 3, comm is NO-CARRY: all comm messages stay
in the caller's shared "local" namespace. Actor addressing uses `from_actor`/`to_actor`
properties on message notes (ADR-057), not namespace partitions. The
`allowed_outbound_namespaces` mechanism below is preserved for the future multi-actor path
(Option B), but is not active in single-namespace deployments.

`send` writes the inbound copy into the recipient's namespace. Whether this write is allowed
depends on the **sender-side outbound allowlist** (`actor.allowed_outbound_namespaces` in the
sender's `khive.toml`). This is an explicit, fail-closed control: the field is empty by
default, so all cross-namespace sends are denied unless the sender opts in.

When a recipient namespace appears in the sender's allowlist, `dual_write_message` mints a
narrowed `NamespaceToken` (via `NamespaceToken::with_namespace`) scoped to the recipient
namespace and uses it to write the inbound note, keeping the write operation namespace-isolated.
The minted token has `namespace = recipient` and `visible = [recipient]`; it is an ordinary
`NamespaceToken` that the comm handler uses in an append-only manner (one `create_note` call,
never returned to the sender). The enforced boundary is the sender-side allowlist check plus
the handler's single-create usage — not the token type. A future multi-actor authorization ADR
will replace this with a type-enforced, append-only capability primitive. The denial error is
`RuntimeError::PermissionDenied { verb: "comm.send" }`.

Within-namespace messaging (sender and recipient in the same namespace) proceeds without any
allowlist check.

The `RuntimeError::CrossNamespaceWrite` variant is retained for the VCS/remote semantics; it
is no longer returned by `comm.send`.

The recipient-side `allowed_inbound_namespaces` (bilateral mutual opt-in) is reserved for a
future release supporting multi-actor deployments and is not part of the current implementation.

#### Message-to-entity attachment

A message can reference a KG entity via `link(message_id, entity_id, annotates)`. This is
the standard `annotates` relation from ADR-002, which accepts any note → any substrate as
the source-target pair. No new edge endpoint rule is required.

#### Storage profile

```rust
impl PackRuntime for CommPack {
    fn storage_profile(&self) -> StorageProfile {
        StorageProfile {
            roles: vec![PlacementRole::Hot],
            default_backend: "main",
        }
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "comm",
            statements: &[
                // idx_comm_message_direction — covers inbox direction + read-status queries.
                // idx_comm_message_thread    — covers thread scans by thread_id.
            ],
        }
    }
}
```

`default_backend="main"` keeps messages on the same backend as kg and gtd data. `Hot` tier
because inbox reads are interactive and latency-sensitive.

#### Comm auxiliary indexes (v1 amendment)

The comm pack registers two partial indexes on the shared notes table to keep `inbox` and
`thread` queries off a full-table scan on high-volume deployments:

| Index                        | Covers                                | Partial condition          |
| ---------------------------- | ------------------------------------- | -------------------------- |
| `idx_comm_message_direction` | `inbox` direction + read-status scans | `WHERE deleted_at IS NULL` |
| `idx_comm_message_thread`    | `thread` scans by `thread_id`         | `WHERE deleted_at IS NULL` |

Both indexes use `WHERE deleted_at IS NULL` (not `WHERE kind = 'message'`) so that SQLite's
query planner can match them when the `kind = ?N` predicate is parameterised. A literal-value
partial index on `kind` cannot be used for a parameterised comparison; the planner sees
different predicates and falls back to a table scan. `deleted_at IS NULL` is present in all
filtered queries, so the partial condition is always satisfied and the index is eligible.

Statements are idempotent (`CREATE INDEX IF NOT EXISTS`) and no auxiliary tables are created.

---

### Part 2: Schedule pack (`khive-pack-schedule`)

#### Event observable vs. `scheduled_event` note kind

The substrate `Event` observable (ADR-004) is a **read-only audit record** emitted by the
runtime on state changes (entity created, note transitioned, edge deleted). It is consumed
by the brain pack (ADR-024) and streaming query surfaces. It is not user-authored.

The schedule pack's note kind is **`scheduled_event`** — a user-authored, future-intent
record. The name is deliberately distinct from the substrate `Event` type to prevent
confusion between the two mechanisms:

| Concept         | Kind / Type              | Author       | Mutability | Purpose               |
| --------------- | ------------------------ | ------------ | ---------- | --------------------- |
| Substrate event | `Event` (not a note)     | Runtime      | Immutable  | Audit / observability |
| Schedule intent | `scheduled_event` (note) | Agent / user | Updateable | Future trigger intent |

Callers create `scheduled_event` notes by calling `remind` or `schedule`. The runtime or an
external scheduler reads these notes, evaluates the trigger time, dispatches the stored
payload, and updates the note's status.

#### Pack identity

```rust
// crates/khive-pack-schedule/src/lib.rs
pub struct SchedulePack { ... }

impl Pack for SchedulePack {
    const NAME:         &'static str            = "schedule";
    const NOTE_KINDS:   &'static [&'static str] = &["scheduled_event"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS:     &'static [HandlerDef]   = &[
        HandlerDef { name: "schedule.remind",   description: "Create a time-triggered reminder.",  visibility: Visibility::Verb },
        HandlerDef { name: "schedule.schedule", description: "Schedule a future verb dispatch.",   visibility: Visibility::Verb },
        HandlerDef { name: "schedule.agenda",   description: "List upcoming scheduled events.",    visibility: Visibility::Verb },
        HandlerDef { name: "schedule.cancel",   description: "Cancel a scheduled event.",          visibility: Visibility::Verb },
    ];
    // ADR-023 §4: pack-prefixed verb names — `schedule.remind` / `schedule.schedule` / `schedule.agenda` / `schedule.cancel`
    const EDGE_RULES:   &'static [EdgeEndpointRule] = &[];
    const REQUIRES:     &'static [&'static str] = &["kg"];
}
```

#### Notes-as-scheduled-events

A `scheduled_event` is a note. `properties` carries the scheduling metadata:

```json
{
  "trigger_at": "2026-05-23T14:00:00Z",
  "repeat": "daily",
  "status": "pending",
  "event_type": "remind",
  "payload": null,
  "fired_at": null,
  "cancelled_at": null
}
```

`event_type` distinguishes `remind` (no action payload; fires a notification) from
`schedule` (stores a serialized verb+args payload for replay). `payload` is null for
reminders and a JSON-encoded verb call string for scheduled dispatch.

#### Four verbs

| Verb                | Speech act (ADR-025) | Args                       | What it does                                                                                                                                                                            |
| ------------------- | -------------------- | -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `schedule.remind`   | commissive           | `content`, `at`, `repeat?` | Create a `scheduled_event` note with `event_type="remind"`. `content` is the reminder body. `at` is ISO 8601. `repeat` is optional recurrence.                                          |
| `schedule.schedule` | commissive           | `action`, `at`, `repeat?`  | Create a `scheduled_event` note with `event_type="schedule"`. `action` is a serialized verb+args payload (a string accepted by the request DSL parser). `at` and `repeat` are as above. |
| `schedule.agenda`   | assertive            | `from?`, `to?`, `limit?`   | List `scheduled_event` notes with `status="pending"`, ordered by `trigger_at` ascending. `from` / `to` are ISO 8601 window bounds. Default `limit=20`.                                  |
| `schedule.cancel`   | declaration          | `id`                       | Set `properties.status = "cancelled"` and record `cancelled_at`. Returns the updated event envelope.                                                                                    |

#### Recurrence specification

`repeat` accepts:

| Value                     | Semantics                                   |
| ------------------------- | ------------------------------------------- |
| `"daily"`                 | Repeat every 24 hours from `trigger_at`     |
| `"weekly"`                | Repeat every 7 days                         |
| `"monthly"`               | Repeat on the same day-of-month each month  |
| cron expression (5-field) | Standard cron: `"0 9 * * 1"` (Monday 09:00) |

No sub-minute precision. The pack validates cron expressions at write time and returns
`RuntimeError::InvalidInput` for malformed expressions.

#### Trigger evaluation and execution

Trigger evaluation — reading pending `scheduled_event` notes, checking `trigger_at` against
the current time, and dispatching the stored payload — is **not performed by the pack in
process**. The pack stores intent. Two supported execution modes:

1. **`kkernel scheduler` daemon mode** (future): `kkernel scheduler --db <path>` polls
   pending events and dispatches them via the internal verb registry. This mode is deferred
   to a future implementation ADR.
2. **External scheduler integration**: An operator configures OS cron or an external scheduler
   to call `kkernel exec --pending-events` at an appropriate polling interval (minimum 1
   minute). The command fetches `schedule.agenda()`, dispatches due events, and marks them `fired`.

The pack's responsibility ends at intent storage. Agents call `remind` or `schedule`; the
execution environment decides when and how to evaluate triggers.

#### `schedule` payload security

The `action` payload accepted by `schedule` is a verb+args string interpreted by the request
DSL parser (ADR-016). The payload runs with the permissions of the namespace that created the
scheduled event — the same authorization gate (ADR-018) applies at dispatch time as at write
time. Agents cannot escalate privileges by storing a payload with a different actor identity.

#### Storage profile

```rust
impl PackRuntime for SchedulePack {
    fn storage_profile(&self) -> StorageProfile {
        StorageProfile {
            roles: vec![PlacementRole::Hot],
            default_backend: "main",
        }
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "schedule",
            statements: &[
                // Index on trigger_at for schedule.agenda() efficiency.
                "CREATE INDEX IF NOT EXISTS idx_schedule_trigger
                    ON notes(json_extract(properties, '$.trigger_at'))
                    WHERE kind = 'scheduled_event'",
            ],
        }
    }
}
```

The partial index on `trigger_at` makes `schedule.agenda()` scans efficient without a new table.
Per ADR-015, pack-auxiliary DDL uses idempotent `CREATE ... IF NOT EXISTS`.

---

### Part 3: Cross-pack interaction

Both packs compose with the existing pack set:

**Schedule + GTD**: a `scheduled_event` can fire a GTD verb at trigger time. For example:
`schedule.schedule(action="gtd.transition(id='abc12345', status='active')", at="2026-06-01T09:00:00Z")`
auto-transitions a task to active at the scheduled time. No coupling at the pack level —
the interaction is at the `action` payload level.

**Schedule + Comm**: a scheduled message is a `scheduled_event` with
`action="comm.send(to='agent:ocean', content='weekly status update')"`. At trigger time the
execution environment dispatches the `comm.send` verb. No coupling at the pack level.

**Comm + KG**: messages attach to KG entities via `link(message_id, entity_id, annotates)`.
The `annotates` relation from ADR-002 accepts any note → any substrate; no new edge endpoint
rule is required for either pack.

**Recall across packs**: `scheduled_event` and `message` notes participate in the hybrid FTS5

- vector search pipeline (ADR-012) like any other note kind. `search(kind="note", query="...")`
  surfaces messages and scheduled events alongside tasks and observations. The `inbox` and
  `agenda` verbs are not the only path to their respective note kinds.

### Part 4: Pack registration

Both packs are Rust packs (not declarative vocabulary packs) because they require verb
handlers with business logic. They self-register via `inventory::submit!` (ADR-027):

```rust
inventory::submit!(Box::new(CommPack::default()) as Box<dyn Pack>);
inventory::submit!(Box::new(SchedulePack::default()) as Box<dyn Pack>);
```

Both declare `REQUIRES = ["kg"]` — the kg pack must be loaded first (ADR-017 boot-time
dependency check). Both use `default_backend = "main"` — no separate backend.

Loading is opt-in via `RuntimeConfig::packs`:

```bash
KHIVE_PACKS=kg,comm          kkernel mcp   # communication only
KHIVE_PACKS=kg,schedule      kkernel mcp   # scheduling only
KHIVE_PACKS=kg,gtd,comm,schedule kkernel mcp   # full stack
```

ADR-016's dynamic verb catalog reflects exactly what is loaded; agents that do not load
`comm` see no `send`/`inbox`/`read`/`reply`/`thread` verbs, and agents that do not load
`schedule` see no `remind`/`schedule`/`agenda`/`cancel` verbs.

## Rationale

### Why notes for both packs

Messages and scheduled events are user-authored records with content, optional tags, a
namespace, and a creation timestamp. This is exactly the notes substrate. Adding new SQL
tables would require new store traits, migrations, query paths, and FTS5 registrations for
what `properties` JSON already handles. The notes substrate already supplies everything both
packs need.

Cross-pack search (`search(query="weekly status")` surfacing both `message` and
`scheduled_event` notes) is free because all note kinds ride the same FTS5 + vector pipeline.

### Why `scheduled_event` and not `event`

The substrate `Event` type (ADR-004) is a read-only system audit observable — runtime-emitted
on every state change, consumed by the brain pack, used for replay and observability. Naming
the schedule pack's note kind `event` would create an immediate terminology collision in any
document, API description, or agent conversation that mentions both.

`scheduled_event` is self-describing: it is a scheduled, future-intent record, not a
historical audit record.

### Why mailbox model for comm

Real-time delivery requires pubsub infrastructure, persistent connections, delivery guarantees,
and retry mechanics — none of which belong in the pack layer. Agents operate at agent-scale
(seconds to minutes per turn), not millisecond-latency. A mailbox model matches the actual
interaction cadence and avoids hard infrastructure dependencies in the binary. Operators
who need real-time delivery build atop the mailbox by polling.

### Why intent-only for schedule

The pack cannot know what polling infrastructure exists in the execution environment. A pack
that tries to own trigger evaluation requires in-process threads, signal handling, and
graceful shutdown — machinery that belongs in the runtime binary, not a pack. Separating
intent storage (pack) from trigger evaluation (runtime/external) keeps the pack composable
across deployment modes: single-binary local use, daemon mode, external cron.

### Why five verbs for comm, four for schedule

The comm pack's natural CRUD shape maps to five verbs: `send`/`reply` are the two creation
paths (standalone vs. threaded), `inbox` and `read` are the two read-path verbs (list and
acknowledge), and `thread` is the conversation-reconstruction verb. `thread` was promoted from
the `list(kind=message, thread_id=X)` workaround path to a first-class verb because (a) it
validates the root ID before scanning, (b) it uses a paginated scan rather than a bounded
prefetch window, and (c) it returns chronologically sorted output — semantics that `list` does
not guarantee.

The schedule pack retains exactly four verbs: `remind`/`schedule` are the two creation paths
(notification vs. verb dispatch), `agenda` is the query verb, and `cancel` is the termination
verb.

Both packs use disjoint verb names with no overlap with kg, gtd, or memory verb names.
ADR-017's `VerbRegistry` rejects duplicates at boot. The total catalog grows by nine verbs
across the two packs (five comm + four schedule), not eight.

### Why no `forget` / `unschedule` — use `cancel` / `delete`

`cancel` on a scheduled event is semantically distinct from `delete` — it marks intent as
deliberately withdrawn while preserving the record (audit trail for "this was scheduled and
then cancelled"). For messages, there is no `withdraw` — delivered messages follow ADR-014's
standard `delete(id)` path.

## Alternatives Considered

| Alternative                                      | Why rejected                                                                                                                                               |
| ------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Embed comm in GTD (`message` as a task variant)  | Conflates communication with task lifecycle; `inbox` semantics are mailbox-oriented, not GTD-lifecycle-oriented; pollutes the GTD verb set                 |
| Use `event` as the schedule note kind            | Terminology collision with substrate `Event` observable (ADR-004); confuses pack-level API readers                                                         |
| Real-time comm delivery via pubsub               | Hard infrastructure dependency in the binary; incompatible with single-process deployment; agent-scale interaction does not require sub-second delivery    |
| In-process trigger loop in the schedule pack     | Couples pack to runtime threading model; prevents use in single-turn call mode; execution environment varies                                               |
| Declarative pack format (ADR-023) for both packs | Both packs require verb handlers with business logic; declarative format applies to vocabulary-only packs                                                  |
| Single combined `commsched` pack                 | Domain cohesion: communication and scheduling are independent concerns; callers that need only comm pay no schedule cost                                   |
| `schedule` payloads run with elevated privileges | Privilege escalation via stored payload; auth gate must apply at dispatch time with the creator's credentials                                              |
| Auxiliary tables for message indexing in v1      | FTS5 + partial expression index on `properties` fields is sufficient at personal/agent scale; auxiliary tables deferred until benchmarked need             |

## Consequences

### Positive

- Two new note kinds (`message`, `scheduled_event`) integrate into the existing notes pipeline
  at zero schema cost. FTS5 search, hybrid recall, and graph linkage work without new plumbing.
- The verb catalog grows by nine verbs across two packs (five comm: send/inbox/read/reply/thread;
  four schedule: schedule/remind/agenda/cancel), each with a distinct and coherent domain.
  ADR-016's dynamic catalog means agents that don't load these packs see no surface bloat.
- The `annotates` edge mechanism from ADR-002 works for both packs without new edge endpoint
  rules — messages and scheduled events attach to KG entities the same way observations do.
- Cross-pack scheduling (GTD, Comm) is composition at the payload level — no inter-pack API.
- The disambiguation between substrate `Event` (ADR-004) and `scheduled_event` note kind is
  explicit in the ADR and enforced by naming.

### Negative

- `inbox` performance at large message volumes depends on a filtered scan on notes where
  `kind="message"` and `properties.direction="inbound"`. At thousands of messages, a
  promoted column or auxiliary index will be needed. Deferred until benchmarked.
- Trigger evaluation for scheduled events is out of scope for the pack. Operators must wire
  either daemon mode (future) or an external scheduler. This is the correct separation but
  creates an operator onboarding step not present in the other three packs.
- Cross-namespace messaging is gated on the sender's `actor.allowed_outbound_namespaces`
  allowlist (specified 2026-06-15; see "Cross-namespace messaging" section above). The field
  defaults to empty, preserving the prior deny-all behavior for existing deployments.
  Within-namespace messaging is unblocked.

### Neutral

- No new edge endpoint rules required. Both packs use `annotates` from the base contract.
- No schema migration needed for either pack. `scheduled_event` and `message` are new values
  in `note.kind`; no DDL change to the notes table.
- The schedule pack's auxiliary index (`idx_schedule_trigger`) is a pack-auxiliary DDL item
  per ADR-015 — idempotent, non-evolving in v1.
- Both packs are additive. Existing kg, gtd, and memory data are unaffected.

## Open Questions

1. **Comm delivery receipts**: Should `send` return a delivery status? Current design returns
   the sent message's ID. Whether the recipient namespace actually exists is a separate check
   that may or may not be surfaced to the sender.

2. **Scheduled event fired status**: After trigger evaluation, the execution environment
   updates `properties.status = "fired"` and `properties.fired_at`. This write must use
   `update(id, properties={...})`. Should the pack register a `fire` verb owned by the
   runtime/daemon, or is `update` sufficient?

3. **Repeat semantics after firing**: For recurring events, the execution environment
   calculates the next `trigger_at` from the `repeat` rule and creates a new `scheduled_event`
   note. Alternatively, the existing note is updated in-place. The correct behavior for
   `cancel` on a recurring event (cancel just the next occurrence vs. all future occurrences)
   is unspecified in v1.

4. **Message namespace write path**: `comm.send(to="agent:khive")` must resolve `agent:khive` to
   a namespace and write a note into that namespace. The exact resolution contract (namespace
   registry, alias table, or unresolved string) is deferred to ADR-018's namespace authority.

   _Resolved 2026-06-15: see "Cross-namespace messaging" section above._

## Implementation

- `crates/khive-pack-comm/src/lib.rs`: `CommPack` struct + `Pack` / `PackRuntime` impls.
- `crates/khive-pack-comm/src/handlers.rs`: `send`, `inbox`, `read`, `reply` handlers;
  direction assignment logic; thread root resolution.
- `crates/khive-pack-schedule/src/lib.rs`: `SchedulePack` struct + `Pack` / `PackRuntime`
  impls.
- `crates/khive-pack-schedule/src/handlers.rs`: `remind`, `schedule`, `agenda`, `cancel`
  handlers; cron validation; trigger-time payload storage.
- `crates/khive-pack-schedule/src/schema.rs`: `idx_schedule_trigger` DDL.
- `crates/kkernel/src/server.rs` (or pack registration): conditional `CommPack` and
  `SchedulePack` registration from `RuntimeConfig::packs`.

## References

- ADR-002: Edge Ontology — `annotates` relation for message and scheduled_event attachment
  to KG entities.
- ADR-004: Substrate Observables — `Event` type is read-only audit; distinct from
  `scheduled_event` note kind defined here.
- ADR-013: Note Kind Taxonomy — adds `message` and `scheduled_event` as pack-extensible
  kinds.
- ADR-014: Curation Operations — standard `delete` for message removal; `update` for
  status mutation on scheduled events.
- ADR-015: Schema Migrations — pack-auxiliary DDL uses idempotent `CREATE ... IF NOT EXISTS`.
- ADR-016: Request DSL — verb dispatch surface that routes to both packs; `action` payload
  in `schedule` is a DSL string.
- ADR-017: Pack Standard — `Pack`, `PackRuntime`, `VerbRegistry`, `REQUIRES` dependency
  check — the mechanism both packs use.
- ADR-018: Authorization Gate — cross-namespace messaging ACL deferred here; gate applies at
  `send` write and at scheduled payload dispatch time.
- ADR-019: GTD Pack — parallel lifecycle-shape pack example; cross-pack scheduling
  interaction at payload level.
- ADR-021: Memory Pack — parallel decay-shape pack example; recall pipeline reuse.
- ADR-023: Declarative Pack Format — mentioned but not used; both packs require verb
  handlers and are Rust packs per ADR-017.
- ADR-025: Verb Speech Acts — `send`, `reply`, `remind`, `schedule` are commissive;
  `inbox`, `agenda` are assertive; `read`, `cancel` are declarative.
- ADR-027: Dynamic Pack Loading — self-registration via `inventory::submit!`.
- ADR-028: Pack-Scoped Backends — `default_backend = "main"` for both packs.
