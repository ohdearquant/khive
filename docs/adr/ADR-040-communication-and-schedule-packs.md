# ADR-040: Communication and Schedule Packs

**Status**: accepted (amended 2026-08-01, 2026-08-06, 2026-08-07, 2026-09-21, 2026-09-24 and 2026-09-25)\
**Date**: 2026-05-23 (last amended 2026-09-25)\
**Authors**: khive maintainers

**Proposed amendment**: [inbox and thread limit disclosure](#amendment-proposed-inbox-and-thread-limit-disclosure-2026-09-14)
adds three fields to successful read payloads upon acceptance. Existing decisions
remain accepted; this proposed addition requires acceptance before implementation merges.

**Proposed amendment**: [comm message file attachments](#amendment-proposed-comm-message-file-attachments-2026-10-02)
adds an optional `attachments` list of blob content references to `comm.send` and `comm.reply`,
an `attachments` field to `comm.inbox`, `comm.thread` and `comm.read` results, and two confined
file-transfer verbs on the blob pack. Existing decisions remain accepted; this proposed addition
requires acceptance before implementation merges.

**Accepted amendment**: [monthly recurrence keeps its day of month](#amendment-2026-09-25-monthly-recurrence-keeps-its-day-of-month)
stores a monthly row's anchor so that a clamped short month no longer moves every later
occurrence.

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
4. **Mailbox model for comm.** No network/pubsub delivery mechanism. Agents originally polled
   via `inbox`; the 2026-08-01 amendment adds a bounded process-local long poll without changing
   the durable mailbox model or adding infrastructure dependencies.
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
        HandlerDef { name: "comm.delivered", description: "Confirm the inbound sibling for an outbound UUID.",             visibility: Visibility::Verb },
        HandlerDef { name: "comm.inbox",  description: "List inbound messages for the caller.",                            visibility: Visibility::Verb },
        HandlerDef { name: "comm.read",   description: "Mark an inbound message as read.",                                 visibility: Visibility::Verb },
        HandlerDef { name: "comm.mark_read", description: "Mark inbound messages read, optionally atomically.",             visibility: Visibility::Verb },
        HandlerDef { name: "comm.unread", description: "Count unread inbound messages.",                                  visibility: Visibility::Verb },
        HandlerDef { name: "comm.reply",  description: "Reply to a message, threading linkage.",                           visibility: Visibility::Verb },
        HandlerDef { name: "comm.thread", description: "Retrieve all messages in a conversation thread, chronologically.", visibility: Visibility::Verb },
        HandlerDef { name: "comm.health", description: "Report channel-poll health.",                                      visibility: Visibility::Verb },
        HandlerDef { name: "comm.probe",  description: "Probe for new inbound message metadata.",                          visibility: Visibility::Verb },
        HandlerDef { name: "comm.ingest", description: "Ingest an external channel message.",                              visibility: Visibility::Subhandler },
        HandlerDef { name: "comm.heartbeat", description: "Record channel-poll liveness.",                                 visibility: Visibility::Subhandler },
        HandlerDef { name: "comm.cursor_get", description: "Read a channel cursor.",                                       visibility: Visibility::Subhandler },
        HandlerDef { name: "comm.cursor_commit", description: "Commit a channel cursor.",                                  visibility: Visibility::Subhandler },
    ];
    // Ten public verbs plus four runtime subhandlers; Visibility controls catalog exposure.
    const EDGE_RULES:   &'static [EdgeEndpointRule] = &[];
    const REQUIRES:     &'static [&'static str] = &["kg"];
}
```

#### Notes-as-messages

A `message` is a note. `kind = "message"` is registered with the runtime via `NOTE_KINDS`.
The `properties` JSON column carries message-specific metadata:

```json
{
  "from": "agent:ops",
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

#### Core message lifecycle verbs

| Verb             | Speech act (ADR-025) | Args                                                        | What it does                                                                                                                                                                                                                                                                                                                                                     |
| ---------------- | -------------------- | ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `comm.send`      | commissive           | `to`, `subject?`, `content`, `thread_id?`                   | Create a message note in the recipient's namespace (`direction=inbound`) and an outbound copy in the caller's namespace (`direction=outbound`). `from` is set to the caller's identity. Both writes are atomic: if the inbound write fails, the outbound copy is rolled back.                                                                                    |
| `comm.inbox`     | assertive            | `limit?`, `offset?`, `box?`, `fields?`, `wait_ms?`, filters | List inbound messages (`direction=inbound`) by default, or caller-authored outbound rows with `box="sent"`. `status` filters inbox read state; offset pagination, field projection, and box-appropriate actor/time/text filters do not change message state. A bounded long poll (`wait_ms`, 1-30,000) waits only when the fully filtered initial page is empty. |
| `comm.read`      | declaration          | `id?`, `ids?`                                               | Set `properties.read = true` on one or more **inbound** messages. Exactly one of `id` or `ids` is required. Outbound messages cannot be marked read.                                                                                                                                                                                                             |
| `comm.mark_read` | declaration          | `ids`, `atomic?`                                            | Canonical named bulk mark-read. Default best-effort behavior matches `comm.read(ids=...)`; `atomic=true` commits every unique mark in one transaction or none. Message content remains a read through `comm.inbox` / `comm.thread`.                                                                                                                              |
| `comm.reply`     | commissive           | `id`, `content`                                             | Fetch the target message's `thread_id` (or use the message's own UUID as the thread root). Create a new message with the same `thread_id`, `to` set to the other party, `subject` prefixed with `"Re: "` if not already. Uses dual-write for inbound delivery to the recipient.                                                                                  |
| `comm.thread`    | assertive            | `id`, `limit?`, `order?`, `after?`, `fields?`               | Validate and resolve the thread root, enforce actor visibility, deduplicate dual-write copies, then apply cursor filtering, requested order, truncation, and optional field projection. `id` accepts an 8-char short prefix or full UUID.                                                                                                                        |

This table focuses the stateful message lifecycle. The accepted amendments below define the other
four current public verbs (`delivered`, `unread`, `health`, and `probe`); the pack-identity snippet
and current-surface rationale enumerate the complete ten-verb catalog.

#### Inbox pagination, richer filters, and bulk read amendment (2026-08-01)

The proposed [limit-disclosure amendment](#amendment-proposed-inbox-and-thread-limit-disclosure-2026-09-14)
adds response fields to this pagination contract without changing page selection.

`comm.inbox` accepts a zero-based `offset` (default 0) in addition to the existing `limit`
(default 20, maximum 200). The offset is applied to the fully-filtered sequence ordered by
`(created_at DESC, id ASC)`, including filters that must be evaluated after the indexed store
query. The response adds `offset`, `has_more`, and `next_offset`; callers enumerate every match by
passing each non-null `next_offset` into the next otherwise-identical call. Pagination never marks
a message read.

The additive inbox filters are:

- `exclude_from_actor`: exclude an exact sender actor label;
- `since`: inclusive RFC 3339 lower bound on the note's top-level `created_at`;
- `before`: exclusive RFC 3339 upper bound on the note's top-level `created_at`;
- `subject_contains`: case-insensitive, non-empty substring match on `properties.subject`;
- `content_contains`: case-insensitive, non-empty substring match on the message body.

All supplied filters are ANDed. `from_actor` remains mutually exclusive with `from_prefix`, while
`exclude_from_actor` may be combined with either. A missing/non-string subject does not match
`subject_contains`. Time filters intentionally use the always-present top-level `created_at` shown
in inbox responses, not the optional transport-origin `properties.sent_at`.

`comm.read` accepts exactly one of `id` or `ids`. `ids` contains 1-500 short prefixes or full
UUIDs. Every target is resolved and checked for namespace, message kind, inbound direction, and
addressee before the first write; duplicate resolved IDs are updated once. A bulk response returns
ordered `results` plus `requested_count`, `unique_count`, `marked_count`, and `failed_count`. Each result uses the
single-message response shape; an update failure carries `read=false` and `mark_error`. Bulk writes
are not promised to be one transaction: any validation failure rejects the operation before writes,
but a later storage failure does not roll back an earlier successful result. The single-`id`
response remains unchanged.

#### Sent-history and list-read projection amendment (2026-08-01)

`comm.inbox` adds `box="sent"` without changing its omitted/default inbound
behavior. The sent box requires `direction=outbound` and a `from_actor` match
to the calling actor; `to_actor` optionally filters the recipient. Attributed
callers fail closed on legacy outbound rows with no `from_actor`, while the
anonymous `local` single-actor fallback retains those rows. Inbox-only sender
and read-status filters are rejected on the sent box instead of being ignored.

`comm.inbox` and `comm.thread` share one non-empty `fields` projection over the
message view. The closed vocabulary includes existing top-level fields and
stable property aliases such as `from_actor`, `to_actor`, and `sent_at`.
Unknown fields are errors. Omission preserves the full response. Projection is
the final presentation step: actor visibility, filters, pagination counts,
thread deduplication, and ordering continue to use the complete record.

Upon acceptance of the proposed [limit-disclosure amendment](#amendment-proposed-inbox-and-thread-limit-disclosure-2026-09-14),
`fields` continues to project message records only; it does not remove the three
new response-level fields from either `comm.inbox` or `comm.thread`.

#### Message-filter scan cap

`list(kind=message, direction=…)` and similar filtered calls route through the KG pack's
paginated scan path. The scan reads the note store in 200-row pages (newest-first) and applies
in-memory filters until `limit` matches are collected. To bound worst-case cost on very large
stores (e.g. 1 M+ messages), the scan stops after at most **10 000 unfiltered rows**
(`MAX_SCAN_TOTAL` in `khive-pack-kg/src/handlers/list.rs`).

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

| Concept         | Kind / Type              | Author       | Mutability       | Purpose               |
| --------------- | ------------------------ | ------------ | ---------------- | --------------------- |
| Substrate event | `Event` (not a note)     | Runtime      | Immutable        | Audit / observability |
| Schedule intent | `scheduled_event` (note) | Agent / user | Schedule-managed | Future trigger intent |

Callers create `scheduled_event` notes by calling `remind` or `schedule`. The runtime or an
external scheduler reads these notes, evaluates the trigger time, dispatches the stored
payload, and updates the note's status. ADR-119 Amendment 4 tightens the original
"updateable" description: generic KG update/merge cannot mutate executable schedule rows,
because retaining immutable creator provenance while changing payload or lifecycle state
would create a confused deputy. `schedule.cancel` and the executor's state-machine
transitions are the supported executable-state mutation paths; generic deletion remains
record removal rather than schedule amendment.

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
  "created_by_actor": "lambda:owner",
  "payload": null,
  "fired_at": null,
  "cancelled_at": null
}
```

`event_type` distinguishes `remind` (no action payload; fires a notification) from
`schedule` (stores a serialized verb+args payload for replay). Both creation paths
mirror `created_by_actor` for display. Replay authority does not come from that mutable
property: the handler stages the note as `provisioning`, writes a target-bound creator
event to the append-only event substrate from its dispatch token, then activates the note
as `pending`. `payload` is null for reminders and a JSON-encoded verb call string for
scheduled dispatch. Scheduled payloads may name only public `Visibility::Verb` handlers;
delayed execution does not grant access to internal subhandlers.

#### Four verbs

| Verb                | Speech act (ADR-025) | Args                       | What it does                                                                                                                                                                                                                                                                                                                                                                            |
| ------------------- | -------------------- | -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `schedule.remind`   | commissive           | `content`, `at`, `repeat?` | Create a `scheduled_event` note with `event_type="remind"`. `content` is the reminder body. `at` is ISO 8601. `repeat` is optional recurrence.                                                                                                                                                                                                                                          |
| `schedule.schedule` | commissive           | `action`, `at`, `repeat?`  | Create a `scheduled_event` note with `event_type="schedule"`. `action` is a serialized verb+args payload — a single, exactly-registered pack-prefixed verb call with only literal args and all required args present (issue #461; stricter than plain request-DSL parseability, since it must survive trigger-time replay unmodified). `at` and `repeat` are as above.                  |
| `schedule.agenda`   | assertive            | `from?`, `to?`, `limit?`   | List `scheduled_event` notes with `status="pending"`, ordered by `trigger_at` ascending. `from` / `to` are ISO 8601 window bounds. Default `limit=20`. On the MCP surface, the loop-owning host additionally decorates the result with process-local `ticker.last_tick_at` under [ADR-106 Amendment D](ADR-106-schedule-pack-executor.md); the pack handler itself remains intent-only. |
| `schedule.cancel`   | declaration          | `id`                       | Set `properties.status = "cancelled"` and record `cancelled_at`. Returns the updated event envelope.                                                                                                                                                                                                                                                                                    |

#### Recurrence specification

`repeat` accepts:

| Value       | Semantics                                  |
| ----------- | ------------------------------------------ |
| `"daily"`   | Repeat every 24 hours from `trigger_at`    |
| `"weekly"`  | Repeat every 7 days                        |
| `"monthly"` | Repeat on the same day-of-month each month |

The pack returns `RuntimeError::InvalidInput` for every other expression. In particular,
five-field cron is rejected because the pending-events executor cannot compute its next
occurrence; accepted recurrence must never degrade silently to a one-shot.

This subsection is superseded, first by the 2026-08-07 amendment and then by the
[2026-09-21 amendment](#amendment-2026-09-21-interval-and-cron-recurrence-through-one-parser),
whose table is the accepted grammar: it admits interval and five-field cron forms and states
`monthly` as the previous trigger plus one calendar month, with month-end clamping.

#### Trigger evaluation and execution

Trigger evaluation — reading pending `scheduled_event` notes, checking `trigger_at` against
the current time, and dispatching the stored payload — is **not performed by the pack in
process**. The pack stores intent. Two supported execution modes:

1. **Warm-daemon mode**: the ADR-119-supervised `schedule-tick` component polls pending
   events and dispatches them through the daemon's live multi-backend verb registry.
2. **External scheduler integration**: An operator configures OS cron or an external scheduler
   to call `kkernel exec --pending-events` at an appropriate polling interval (minimum 1
   minute). The command fetches `schedule.agenda()`, dispatches due events, and marks them `fired`.

The pack's responsibility ends at intent storage. Agents call `remind` or `schedule`; the
execution environment decides when and how to evaluate triggers.

#### `schedule` payload security

The `action` payload accepted by `schedule` is a verb+args string interpreted by the request
DSL parser (ADR-016). The payload runs with the namespace and persisted actor identity that
created the scheduled event — the same authorization gate (ADR-018) applies at dispatch time
as at write time. The daemon actor is never substituted. Legacy generic rows lacking creator
identity fail closed under ADR-119, so agents cannot escalate privileges by storing a payload
for later replay by a more privileged daemon.

#### Storage profile

```rust
impl PackRuntime for SchedulePack {
    fn storage_profile(&self) -> StorageProfile {
        StorageProfile {
            roles: vec![PlacementRole::Hot],
            default_backend: "main",
        }
    }

    // No auxiliary tables: use the default empty schema plan.
}
```

_Corrected in place on 2026-10-05 (#4011):_ this section and the Neutral note below described the schedule
indexes as pack-auxiliary DDL. That contradicted ADR-015 and ADR-017, which keep pack schema plans
auxiliary-only, and the example also showed an older index definition. Both indexes now belong to core
migration V51 with the definitions the pack already used.

The core index `idx_schedule_trigger` covers
`notes(namespace, kind, json_extract(properties, '$.trigger_at'))` with the partial
condition `deleted_at IS NULL`, allowing parameterized kind predicates to use it.
Migration V51 also owns `idx_schedule_creator_provenance` on
`events(namespace, verb, target_id, outcome)`. Both use idempotent
`CREATE INDEX IF NOT EXISTS`, preserving previously installed definitions and
btrees. Per ADR-015 and ADR-017, indexes on these core tables belong to numbered
core migrations; pack schema plans remain auxiliary-only.

---

### Part 3: Cross-pack interaction

Both packs compose with the existing pack set:

**Schedule + GTD**: a `scheduled_event` can fire a GTD verb at trigger time. For example:
`schedule.schedule(action="gtd.transition(id='abc12345', status='active')", at="2026-06-01T09:00:00Z")`
auto-transitions a task to active at the scheduled time. No coupling at the pack level —
the interaction is at the `action` payload level.

**Schedule + Comm**: `schedule.remind` requires `comm.send` at creation time because
reminders deliver through the comm inbox path. If that capability is not registered,
the handler rejects the call before persisting a note; the rest of the schedule pack
remains available. A scheduled message is a `scheduled_event` with
`action="comm.send(to='agent:ops', content='weekly status update')"`. At trigger time the
execution environment dispatches the `comm.send` verb.

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

Both packs declare `REQUIRES = ["kg"]`, and the ADR-017 boot-time dependency check
enforces that shared substrate requirement. `schedule.remind` separately checks for
the registered `comm.send` delivery capability at creation time and rejects before
writing when it is absent. Both use `default_backend = "main"` — no separate backend.

Loading is opt-in via `RuntimeConfig::packs`:

```bash
KHIVE_PACKS=kg,comm          kkernel mcp   # communication only
KHIVE_PACKS=kg,schedule      kkernel mcp   # scheduling without reminder creation
KHIVE_PACKS=kg,comm,schedule kkernel mcp   # scheduling with reminder creation
KHIVE_PACKS=kg,gtd,comm,schedule kkernel mcp   # full stack
```

ADR-016's dynamic verb catalog reflects exactly what is loaded. Agents that do not load
`schedule` see no `remind`/`schedule`/`agenda`/`cancel` verbs. Loading `schedule`
without `comm` leaves all four verbs registered, but `schedule.remind` rejects at creation
before persistence until `comm.send` is available.

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

The 2026-08-01 amendment below supersedes only the polling-latency conclusion:
`comm.inbox(wait_ms=...)` may now wait on a process-local commit signal. The
database-backed mailbox remains authoritative, and there is still no network
pubsub, persistent delivery socket, or independent delivery-guarantee layer.

### Why intent-only for schedule

The pack cannot know what polling infrastructure exists in the execution environment. A pack
that tries to own trigger evaluation requires in-process threads, signal handling, and
graceful shutdown — machinery that belongs in the runtime binary, not a pack. Separating
intent storage (pack) from trigger evaluation (runtime/external) keeps the pack composable
across deployment modes: single-binary local use, daemon mode, external cron.

### Initial five-verb comm shape and current surface

At this ADR's original acceptance, the comm pack's natural CRUD shape mapped to five verbs:
`send`/`reply` are the two creation
paths (standalone vs. threaded), `inbox` and `read` are the two read-path verbs (list and
acknowledge), and `thread` is the conversation-reconstruction verb. `thread` was promoted from
the `list(kind=message, thread_id=X)` workaround path to a first-class verb because (a) it
validates the root ID before scanning, (b) it uses a paginated scan rather than a bounded
prefetch window, and (c) it returns chronologically sorted output — semantics that `list` does
not guarantee.

Subsequent accepted amendments added `delivered`, `unread`, `health`, `probe`, and `mark_read`.
The current comm catalog therefore exposes ten public verbs. Its 14 handler definitions also
include four runtime subhandlers (`ingest`, `heartbeat`, `cursor_get`, and `cursor_commit`), which
do not appear in the public verb catalog.

The schedule pack retains exactly four public verbs: `remind`/`schedule` are the two creation paths
(notification vs. verb dispatch), `agenda` is the query verb, and `cancel` is the termination
verb.

Both packs use disjoint verb names with no overlap with kg, gtd, or memory verb names.
ADR-017's `VerbRegistry` rejects duplicates at boot. The original decision added nine public verbs
(five comm + four schedule); the current combined public surface is 14 (ten comm + four schedule).

### Why no `forget` / `unschedule` — use `cancel` / `delete`

`cancel` on a scheduled event is semantically distinct from `delete` — it marks intent as
deliberately withdrawn while preserving the record (audit trail for "this was scheduled and
then cancelled"). For messages, there is no `withdraw` — delivered messages follow ADR-014's
standard `delete(id)` path.

## Alternatives Considered

| Alternative                                      | Why rejected                                                                                                                                            |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Embed comm in GTD (`message` as a task variant)  | Conflates communication with task lifecycle; `inbox` semantics are mailbox-oriented, not GTD-lifecycle-oriented; pollutes the GTD verb set              |
| Use `event` as the schedule note kind            | Terminology collision with substrate `Event` observable (ADR-004); confuses pack-level API readers                                                      |
| Real-time comm delivery via pubsub               | Hard infrastructure dependency in the binary; incompatible with single-process deployment; agent-scale interaction does not require sub-second delivery |
| In-process trigger loop in the schedule pack     | Couples pack to runtime threading model; prevents use in single-turn call mode; execution environment varies                                            |
| Declarative pack format (ADR-023) for both packs | Both packs require verb handlers with business logic; declarative format applies to vocabulary-only packs                                               |
| Single combined `commsched` pack                 | Domain cohesion: communication and scheduling are independent concerns; callers that need only comm pay no schedule cost                                |
| `schedule` payloads run with elevated privileges | Privilege escalation via stored payload; auth gate must apply at dispatch time with the creator's credentials                                           |
| Auxiliary tables for message indexing in v1      | FTS5 + partial expression index on `properties` fields is sufficient at personal/agent scale; auxiliary tables deferred until benchmarked need          |

## Consequences

### Positive

- Two new note kinds (`message`, `scheduled_event`) integrate into the existing notes pipeline
  at zero schema cost. FTS5 search, hybrid recall, and graph linkage work without new plumbing.
- The original decision grew the verb catalog by nine verbs across two packs. Accepted amendments
  bring the current public surface to 14 (ten comm and four schedule), each with a distinct and
  coherent domain. ADR-016's dynamic catalog means agents that don't load these packs see no
  surface bloat.
- The `annotates` edge mechanism from ADR-002 works for both packs without new edge endpoint
  rules — messages and scheduled events attach to KG entities the same way observations do.
- Cross-pack scheduling (GTD, Comm) is composition at the payload level — no inter-pack API.
- The disambiguation between substrate `Event` (ADR-004) and `scheduled_event` note kind is
  explicit in the ADR and enforced by naming.

### Negative

- `inbox` performance at large message volumes depends on a filtered scan on notes where
  `kind="message"` and `properties.direction="inbound"`. At thousands of messages, a
  promoted column or auxiliary index will be needed. Deferred until benchmarked.
- Trigger evaluation for scheduled events is out of scope for the pack. The warm daemon's
  ADR-119 component owns the default executor; an external scheduler remains an optional,
  CAS-safe fallback. This preserves the intent/execution separation across crate boundaries.
- Cross-namespace messaging is gated on the sender's `actor.allowed_outbound_namespaces`
  allowlist (specified 2026-06-15; see "Cross-namespace messaging" section above). The field
  defaults to empty, preserving the prior deny-all behavior for existing deployments.
  Within-namespace messaging is unblocked.

### Neutral

- No new edge endpoint rules required. Both packs use `annotates` from the base contract.
- The `scheduled_event` and `message` note kinds require no new table columns.
- Schedule's indexes on `notes` and `events` are core DDL. Migration V51 owns them
  per ADR-015 / ADR-017, independently of schedule pack loading.
- Both packs are additive. Existing kg, gtd, and memory data are unaffected.

## Open Questions

1. **Comm delivery receipts**: Should `send` return a delivery status? Current design returns
   the sent message's ID. Whether the recipient namespace actually exists is a separate check
   that may or may not be surfaced to the sender.

2. **Scheduled event fired status (resolved by ADR-106/ADR-119)**: After trigger evaluation,
   the execution environment updates `properties.status = "fired"` and
   `properties.fired_at` through the schedule executor's conditional state-machine write.
   Generic `update` is not sufficient because it would expose executable intent and creator
   authority to confused-deputy substitution.

3. **Repeat semantics after firing (resolved by ADR-106)**: Named recurring events update the
   existing note in place, advancing `trigger_at` and returning it to `pending`. Cancelling that
   row cancels the recurring schedule. Unsupported recurrence, including five-field cron, is
   rejected at the write boundary rather than silently degrading to one-shot delivery.

4. **Message namespace write path**: `comm.send(to="agent:khive")` must resolve `agent:khive` to
   a namespace and write a note into that namespace. The exact resolution contract (namespace
   registry, alias table, or unresolved string) is deferred to ADR-018's namespace authority.

   _Resolved 2026-06-15: see "Cross-namespace messaging" section above._

## Implementation

- `crates/khive-pack-comm/src/lib.rs`: `CommPack` struct + `Pack` / `PackRuntime` impls.
- `crates/khive-pack-comm/src/handlers.rs`: `send`, `inbox`, `read`, `mark_read`, `reply` handlers;
  direction assignment logic; thread root resolution.
- `crates/khive-pack-schedule/src/lib.rs`: `SchedulePack` struct + `Pack` / `PackRuntime`
  impls.
- `crates/khive-pack-schedule/src/handlers.rs`: `remind`, `schedule`, `agenda`, `cancel`
  handlers; executable recurrence validation; trigger-time payload storage.
- `crates/khive-db/sql/051-schedule-core-indexes.sql`: trigger and creator-provenance indexes on core tables.
- `crates/khive-mcp/src/serve.rs` (pack registration in `build_registry_for_multi_backend*`): conditional `CommPack` and
  `SchedulePack` registration from `RuntimeConfig::packs`.

## References

- ADR-002: Edge Ontology — `annotates` relation for message and scheduled_event attachment
  to KG entities.
- ADR-004: Substrate Observables — `Event` type is read-only audit; distinct from
  `scheduled_event` note kind defined here.
- ADR-013: Note Kind Taxonomy — adds `message` and `scheduled_event` as pack-extensible
  kinds.
- ADR-014: Curation Operations — standard `delete` for message removal; ADR-119 Amendment 4
  supersedes generic `update` for schedule status mutation with schedule-owned transitions.
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
  `inbox`, `agenda` are assertive; `read`, `mark_read`, `cancel` are declarative.
- ADR-027: Dynamic Pack Loading — self-registration via `inventory::submit!`.
- ADR-028: Pack-Scoped Backends — `default_backend = "main"` for both packs.

## Amendment (2026-08-01): `comm.read` degraded mark-read contract

The `comm.read` row in the verb table above (line 102) states unconditionally that the
verb "Set[s] `properties.read = true`" and "Returns the updated message envelope." That
wording is only accurate when the post-read mark-read patch actually lands. Under
multi-client writer contention the mark-read patch can
time out or find no live row to update, and `handle_read` now degrades instead of failing
the whole call: the fetch already succeeded, so a delivery-state patch failure should not
throw away a successful read for a caller who cannot retry the fetch half.

**Corrected contract**: validation errors that run before the patch (not found, wrong
kind, outbound direction, wrong addressee) remain fatal and unchanged. The post-read
mark-read patch itself is best-effort:

The patch uses `NoteStore::try_patch_note_property("read", true)`, an atomic storage-level JSON set
with a live eligibility recheck.
It does not replace the properties document, so a concurrent write to another key survives without
a get/retry loop. This changes no `comm.read` response field or error/degradation rule.

- On success, the response is as originally specified: `read: true`, `properties` is the
  updated envelope.
- On a no-op (no live row updated, e.g. soft-deleted mid-flight) or a storage error, the
  response degrades to `read: false` with a `mark_error` field (a fixed string for the
  no-op case, the error's `Display` string otherwise) and `properties` set to the
  pre-patch stored value — never a value implying the patch landed.

This is eventual consistency on the read flag, not a correctness gap: a caller polling
unread counts sees the message still unread and can simply re-issue `comm.read`
(self-healing, no retry loop needed in the handler). See
`crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_read` for the full
three-arm contract and `crates/khive-pack-comm/src/handlers.rs::read_response` for the
implementation. The `comm.read` discovery description in
`crates/khive-pack-comm/src/vocab.rs` and the public guides (`AGENTS.md`,
`docs/guide/api-reference.md`, `docs/guide/communication.md`, the comm skill) carry the
best-effort wording; the verbatim `HandlerDef` snippet earlier in this ADR reflects the
original pre-amendment description.

## Amendment (2026-08-01): bounded `comm.inbox` long poll (#1499)

The proposed [limit-disclosure amendment](#amendment-proposed-inbox-and-thread-limit-disclosure-2026-09-14)
qualifies only the unchanged-response-schema statement below, adding the same
three fields to every successful return. The deadline, requery and zero-limit
no-wait semantics remain unchanged.

`comm.inbox` accepts optional `wait_ms` in the inclusive range 0 through
30,000. Omission and zero preserve the original immediate snapshot. A positive
value establishes one deadline before the initial query; if that fully scoped
query finds a message, the call returns immediately. Otherwise it waits for a
process-local message-commit signal and re-runs the same namespace, actor,
status, and sender-filtered query. `limit=0` remains an immediate count-only
operation and never waits. The response schema is unchanged.

Each `CommPack` instance owns an `InboxSignal` consisting of
`tokio::sync::Notify` plus a monotonically increasing generation counter. The
inbox handler snapshots the generation before every query, so a commit between
an empty query and waiter registration cannot be lost. The signal is deliberately
payload-free and unscoped: every wake is followed by the normal authorized query,
and an unrelated actor/namespace/filter result causes the call to continue waiting
within its original deadline. A final query at deadline expiry observes commits
visible before that query takes its storage snapshot; a commit landing after the
snapshot is left to the caller's next request.

Successful `comm.send` and `comm.reply` handlers publish only after their atomic
dual-write commits. `comm.ingest` publishes only after `try_create_note` returns a
newly committed note; an `external_id` dedup hit does not publish. Publishing in
the comm handler is both more precise and narrower than teaching each channel
poll loop to interpret an ingest response. It also requires no post-construction
injection: the waiting and writing handlers already execute on the same immutable,
registry-owned `CommPack` instance.

This is an in-process latency optimization, not a second source of delivery
truth. Direct database writes or a distinct process/registry do not share the
signal; the timeout-edge final query or the caller's next request observes them
from durable storage. No notification carries message content, actor identity,
or authorization, and no wake bypasses ADR-018/ADR-057 filtering.

## Amendment (2026-08-06): named atomic mark-read (#1387)

The 0.7.0 surface already shipped bulk best-effort mutation as `comm.read(ids=[...])` through
#1422/#1572. That compatibility contract remains intact, including its single-`id` form, bulk form,
complete target validation before the first write, duplicate-ID collapse, result/count envelope,
and per-target post-validation storage degradation. Removing `ids` from `comm.read` would break a
released wire shape, so it remains a compatibility alias rather than being narrowed back to one id.

Those released decisions supersede the corresponding premises and acceptance bullets in the
original #1387 issue. #1572 already delivered the bulk best-effort operation and established fatal
whole-call prevalidation rather than a new per-entry authorization-error envelope. ADR-057 and
ADR-007 establish actor-addressed eligibility, attribution-only namespaces, and fail-open access to
legacy rows with no `to_actor`; they supersede #1387's earlier wrong-namespace and
attributed-versus-unattributed legacy split. The residual #1387 scope is therefore the canonical
`comm.mark_read` name, its retrieval-versus-mutation catalog wording, and the `atomic=true`
all-or-nothing transaction.

The comm pack adds `comm.mark_read(ids=[...], atomic=false)` as the canonical, unambiguous bulk
name. It marks delivery state; it never retrieves message bodies. Callers retrieve content through
the Assertive `comm.inbox` and `comm.thread` verbs. With omitted/default `atomic=false`, the handler
reuses the existing `comm.read(ids=...)` validation and best-effort mutation path; this amendment
does not fork the authorization or response logic.

`atomic=true` keeps the same 1-500 input cap, prefix/UUID resolution, complete prevalidation,
first-occurrence order, deduplication, and bulk response fields. After prevalidation it passes every
unique UUID plus the same namespace/kind/direction/addressee `NoteFilter` to
`NoteStore::patch_note_property_atomic`. The SQLite implementation rechecks each target inside one
writer transaction and patches only `$.read` through `json_set`. Every statement must affect exactly
one live object-valued row. A missing, soft-deleted, wrong-kind, outbound, wrong-addressee, or
otherwise ineligible row aborts the transaction, as does a statement or commit failure observed by
the transaction executor; no earlier mark in that failed unit remains committed. Both the
writer-task and legacy pool-mutex executors verify commit/rollback completion and restored
autocommit mode. A lost writer-task reply or an unverified transaction finalization returns the
existing `side_effects_unknown` storage error and permanently retires that writer seam: the unit is
still indivisible, but the caller cannot infer whether it committed and the poisoned connection is
never reused. A successful unit returns the existing bulk summary with `read=true` for every unique
target. The 500-id cap also bounds contention: one atomic call holds the single writer across at
most 500 guarded `UPDATE`s, limiting head-of-line latency for concurrent writers.

This amendment does not change ADR-057's actor or legacy-row decisions. In particular,
`to_actor`-less pre-ADR-057 rows retain the accepted `EqOrMissing` fail-open compatibility rule for
both names; newly attributed messages remain addressee-gated. Target-validation errors remain fatal
before either mutation mode, matching the shipped #1572 contract rather than introducing a second,
per-entry authorization-error envelope only on the new spelling.

## Amendment (2026-08-07): executable schedule recurrence

The recurrence grammar is narrowed to `daily`, `weekly`, and `monthly`, exactly the
forms the executor can advance. All cron expressions are rejected at intent creation;
legacy cron rows fail closed before dispatch. This supersedes the earlier limited
five-field grammar in this ADR. Durable occurrence/invocation receipts, renewable
dispatch leases, crash reconciliation, and failed one-shot recovery are governed by
[ADR-106 Amendment F](ADR-106-schedule-pack-executor.md#amendment-f-durable-dispatch-receipts-and-renewable-leases-2026-08-07).

## Amendment (proposed): inbox and thread limit disclosure (2026-09-14)

**Status**: proposed. Acceptance is required before dependent implementation merges.
This amendment adds normalization evidence to `comm.inbox` and `comm.thread`
success payloads. It partially qualifies the payload descriptions in Part 1's
pagination/projection clauses and the long-poll amendment's statement that the
response schema is unchanged. All existing message fields, actor rules,
selection, ordering, pagination, cursor, deduplication and wait behavior remain
unchanged; schedule verbs are outside this amendment.

Each successful canonical payload MUST include these flat fields beside its
existing fields, never inside a message record:

- `requested_limit`: the accepted unsigned integer supplied as `limit`, or the
  verb's numeric default when `limit` is omitted or null.
- `effective_limit`: the normalized limit actually used by the handler.
- `limit_clamped`: the boolean `requested_limit != effective_limit`.

| Verb          | Numeric default | Effective limit                  | Explicit zero report                                    |
| ------------- | --------------: | -------------------------------- | ------------------------------------------------------- |
| `comm.inbox`  |              20 | `min(requested_limit, 200)`      | `0 / 0 / false`; immediate count-only response, no wait |
| `comm.thread` |             100 | `clamp(requested_limit, 1, 500)` | `0 / 1 / true`; existing lower clamp                    |

Both inputs retain their strict `Option<u32>` contract. Negative or fractional
numbers, strings, booleans, arrays, objects and integers above `4294967295` are
errors; this amendment introduces no aliases or coercions. Errors and help
responses MUST NOT carry a successful normalization report. Existing validation
and authorization order is unchanged.

The report describes settings, not the number of returned messages, scan work,
truncation or completeness. A request for 201 inbox messages that finds one row
reports `201 / 200 / true`. Existing `count`, unread counts, `has_more` and
`next_offset` retain their meanings and calculations. In particular, zero inbox
limit still computes the actual unread-count metadata in inbox mode and the
existing zero unread metadata in sent mode. The effective page cap does not bound
how many stored rows post-filtering or thread traversal scans.

All three fields MUST appear on empty as well as populated successful canonical
payloads, including inbox zero, immediate match, wakeup, deadline/final requery,
and a thread exhausted by `after` or actor filtering. Inbox filtering must occur
before logical pagination, and thread visibility/deduplication and requested
ordering must occur before truncation. The report MUST NOT change those steps or
substitute for the lookahead used to calculate continuation fields.

Message `fields` projection remains confined to each record; the report is outside
that projection. Presentation MUST retain the numeric/boolean report, including
zero and false, while preserving existing empty-array/null elision. Auto/table
rendering may express payload scalars in the existing rendered form; it need not
convert an existing string response back to JSON. This amendment adds no stored
message fields and performs no read-state mutation.

Validation MUST compare canonical pre/post payloads after removing only the
three new fields, using defaults/null, zero, cap boundaries and `u32::MAX` in both
inbox boxes and both thread orders. Populated over-cap fixtures must prove the
executed cap as well as the report. Projection, actor/dedup/cursor ties,
SQL-only/post-filter paging, zero-with-positive-wait, immediate/wakeup/deadline
returns, and Agent/Human/Verbose with JSON/Auto/Table remain regression controls.
The existing forced final-requery library tests remain intact; public handler
controls verify report propagation through the final successful return.
Mutations that omit a success branch's fields, report requested as effective,
execute the raw limit, use `>` instead of `!=`, or project away response metadata
MUST be detected. These are pending acceptance requirements, not test results.

This proposal preserves the actor and legacy-row contracts in
[ADR-057](ADR-057-comm-actor-addressed-delivery.md) and
[ADR-063](ADR-063-comm-principal-model.md); it creates no new authorization seam.
Other verbs' limits and response contracts are unchanged.

## Amendment (2026-09-21): interval and cron recurrence through one parser

This amendment supersedes the 2026-08-07 amendment above, which narrowed `repeat` to
`daily`, `weekly`, and `monthly` because the executor could advance only those forms.
That constraint no longer holds: since #2484 schedule creation and the pending-events
drain share one parser, `khive_pack_schedule::repeat`, and the executor advances every
form that parser accepts.

`repeat` accepts:

| Value                     | Semantics                                                                                   |
| ------------------------- | ------------------------------------------------------------------------------------------- |
| `"daily"`                 | The previous trigger plus one day                                                           |
| `"weekly"`                | The previous trigger plus seven days                                                        |
| `"monthly"`               | The previous trigger plus one calendar month, with month-end clamping                       |
| `"every:<N><s\|m\|h\|d>"` | A fixed interval of `N` seconds, minutes, hours or days from the previous trigger, `N >= 1` |
| five-field cron           | The next match after the previous trigger, evaluated in UTC                                 |

Any other value is rejected at creation with an error that names the rejected value and
the accepted forms. Creation also rejects a parsed recurrence that has no representable
occurrence after its requested `at`, naming the rejected value. A legacy row whose stored
`repeat` the parser does not accept still
fails closed before invocation; the fail-closed rule is unchanged, only the accepted
grammar widened. The write boundary and the executor cannot disagree about what a
recurrence means because there is exactly one definition of it; that property, not any
particular grammar, is what this ADR guarantees.

Missed-occurrence advancement for the new forms is specified in
[ADR-106 Amendment G](ADR-106-schedule-pack-executor.md#amendment-g-interval-and-cron-recurrence-2026-09-21).

## Amendment (2026-09-24): `comm.read` returns the message on a successful mark

This amendment addresses #1797 and supersedes the older `comm.read` descriptions
above that call it acknowledgement-only. The existing mark operation, including
single and 1–500 ID forms, complete prevalidation, duplicate resolution, guarded
patch, per-item best-effort degradation, and aggregate counts, is unchanged.

`comm.read(id=...)` and `comm.read(ids=[...])` accept an optional boolean `body`
that defaults to true. When the mark for a validated inbound message succeeds,
its result retains every existing acknowledgement field and adds the message's
top-level `subject`, `content`, `from`, `to`, `direction`, and `created_at` fields.
These use the same value conventions as `comm.inbox`; a missing subject is JSON
null. Each successful unique bulk result receives its own fields, in the same
order as its existing result row. The body comes from the note fetched during
prevalidation; the guarded mark must succeed before those fields are exposed.
Because the body is that prevalidation snapshot, a concurrent edit can leave the
returned content older than the message at the moment of the mark.

`body=false` retains the prior single or bulk response shape while still
attempting the mark. In particular it adds no top-level message fields; the
existing `properties` field is not altered. A failed or indeterminate mark adds
no top-level message fields regardless of `body`, and a validation or permission
failure remains an error without a message result. This does not change
`comm.mark_read`, which remains acknowledgement-only in both best-effort and
atomic modes. `comm.inbox` and `comm.thread` retain their current payload and
projection behavior; changing their defaults is outside this amendment.

Acceptance requires a default single read with subject/body and routing, a
successful bulk read with distinct bodies and a duplicate ID, the `body=false`
single and bulk opt-outs, an actor refusal with no content, degraded mark rows
with no new message fields, and an unchanged `comm.mark_read` response. A
mutation that drops the body, ignores the opt-out, or leaks fields on failure
must make a targeted control fail.

## Amendment (2026-09-25): monthly recurrence keeps its day of month

**Status**: accepted (2026-09-25).

### Context

The 2026-09-21 amendment states `monthly` as "The previous trigger plus one calendar month, with
month-end clamping", and the code does exactly that. `Repeat::next_after` in
`crates/khive-pack-schedule/src/repeat.rs` computes `current.checked_add_months(Months::new(1))`,
`Repeat::first_after` steps through the same function for missed occurrences, and the executor
(`next_trigger_at` and `advance_repeat_past_missed` in `crates/khive-mcp/src/pending_events.rs`)
stores the result back as the row's `trigger_at`. The clamped date becomes the base of the next step,
so a series created for the 31st runs Jan 31, Feb 28, Mar 28, Apr 28 and stays on the 28th from then
on (#3322). Nothing on the row remembers the requested day: creation stores `trigger_at` and `repeat`
(`crates/khive-pack-schedule/src/handlers.rs`), the executor overwrites `trigger_at` on every
advance, and the creator provenance event records the event type but not the trigger.

The base text of this record defined `monthly` as "Repeat on the same day-of-month each month", and
the schedule pack's design document still does. The 2026-09-21 amendment was written to record that
creation and the executor share one parser and to widen the grammar; its `monthly` row describes the
implementation and gives no reason for the drift. This amendment decides the question on its merits.

### Decision

1. **`monthly` keeps its anchor's day of month.** The occurrences of a `monthly` row are its anchor
   plus n calendar months, for n = 1, 2, ..., each clamped to the last day of its own month. A clamped
   date is never the base of the next step: a row anchored on Jan 31 runs Feb 28 (29 in a leap year),
   Mar 31, Apr 30, May 31. Time of day is the anchor's, and month arithmetic stays in UTC as today.
2. **The anchor is stored.** Creating a `monthly` row records its creation `trigger_at` in a
   schedule-managed property, `repeat_anchor`, beside `trigger_at`. It is written once and never
   advanced. Generic update and merge already refuse schedule-managed notes, so it has the same
   protection as `trigger_at`.
3. **Advancement reads the anchor.** Normal advancement arms the first anchored occurrence strictly
   after the row's current `trigger_at`. Missed-occurrence advancement arms the first anchored
   occurrence strictly after `now`, stepping one occurrence at a time as ADR-106 Amendment G
   specifies for the calendar aliases, and the missed occurrence itself is recorded as missed and
   never dispatched. Both are computed from the row's own stored data; the tick's observed time is a
   bound, never a base.
4. **Rows without an anchor.** A `monthly` row created before this amendment has no
   `repeat_anchor`. Its current `trigger_at` is used as the anchor, and the executor writes that
   value as `repeat_anchor` in the same finalization that advances the row. A row that has not yet
   clamped keeps its day from then on. A row that already clamped cannot be restored, because the
   requested day is recorded nowhere on it; it stays on its current day, which is what it does today,
   and does not drift further.
5. **The other forms are unchanged.** `daily`, `weekly` and `every:` are fixed durations with nothing
   to clamp, and cron occurrences are absolute positions of the pattern. None of them reads or writes
   `repeat_anchor`.
6. **Invalid stored anchors fail visibly.** If a stored `repeat_anchor` is not a timestamp or is
   later than the row's current `trigger_at`, the executor fails the row without advancing it.
   It records the calendar error in `recurrence_error` beside any action delivery or dispatch
   error. An already durable action receipt keeps its known outcome; calendar failure does not
   classify that invocation as indeterminate. A missed occurrence still records its missed
   receipt without dispatching. The failed row is counted once after finalization commits.

Acceptance, stated before any implementation runs:

1. A row anchored on Jan 31 advances through Feb 28, Mar 31 and Apr 30 under normal advancement.
2. A row whose Jan 31 occurrence is missed until a `now` in mid-March records Jan 31 as missed and
   arms Mar 31. Mutation control: advancing from the previous `trigger_at` instead of the anchor turns
   arms 1 and 2 red at Mar 28.
3. A legacy row without `repeat_anchor` whose `trigger_at` is Jan 31 advances to Feb 28 and then to
   Mar 31, and carries `repeat_anchor` after its first advance. A legacy row whose `trigger_at` is
   Feb 28 advances to Mar 28.
4. A generic `update` that tries to change `repeat_anchor` is refused as schedule-managed.
5. `daily`, `weekly`, `every:` and cron fixtures advance exactly as before.

### Alternatives considered

- **Keep chained clamping** and align the design document with it. No stored field and no executor
  change. It keeps a behaviour that no calendar convention uses: after the first short month the
  requested day is lost for good, the row carries no record of it, and the person the reminder is for
  cannot tell from the reminder that it moved.
- **Skip months that lack the anchor day** (the iCalendar `BYMONTHDAY` reading). Nothing needs
  clamping, but a series on the 31st fires in seven months of the year, and a `monthly` row stops
  meaning one occurrence per month.
- **Derive the anchor instead of storing it.** `created_at` is when the row was written, not the
  requested trigger, and the provenance event does not carry the trigger, so no existing field holds
  the value.

### Consequences

- One new schedule-managed property on `monthly` rows. It lives in the note's properties, so there
  is no schema migration.
- The 2026-09-21 amendment's `monthly` row and ADR-106 §7 ("computed from the row's own stored
  `trigger_at`") read together with this amendment: the anchor becomes part of the stored data the
  calendar step uses. ADR-106 Amendment F derives an occurrence's identity from the event id and the
  scheduled instant, so each occurrence still has exactly one deterministic identity; an anchored
  occurrence has a different instant (Mar 31) from the chained one (Mar 28).
- The schedule pack's design document already describes `monthly` as "same day-of-month", which this
  amendment makes accurate.
- Rows that clamped before the change keep their clamped day.

Refs: #3322.

## Amendment (proposed): comm message file attachments (2026-10-02)

**Status**: accepted (2026-10-02, ratified by the maintainer).

The requirement is "file bytes never enter a tool result", with files moved on a local server by
`blob.import(path)` and `blob.export(content_ref, path)` and on a remote MCP surface by "refs plus
short-lived signed HTTPS URLs (PUT for upload, GET for download)"; this amendment specifies the
local half only and defers the remote half, signed-URL transfer, to a later amendment of
[ADR-105](ADR-105-cross-node-comm-transport.md), as listed under [Out of scope](#out-of-scope).

This amendment lets a message carry files by reference. `comm.send` and `comm.reply` accept an
optional `attachments` list of blob content references. `comm.inbox`, `comm.thread` and `comm.read`
return each message's references with their sizes. File bytes never appear in a comm request or
result. Bytes move through the blob pack: the existing upload verbs and `blob.get`, plus two new
verbs, `blob.import` and `blob.export`, that move a file between the server's disk and the blob
store inside configured directories. A message sent without attachments behaves exactly as it did
before. Core migration 047 preserves rows with invalid roles in `attachment_quarantine` and
tightens the attachment role constraints.

The "Message-to-entity attachment" section above covers linking a message to a knowledge-graph
entity with the `annotates` relation. It is a different feature and is unchanged. In this amendment
"attachment" always means a file reference carried by a message.

### Data model

1. **A message holds references.** An attachment is the BLAKE3 content reference of an object in
   the blob store ([ADR-111](ADR-111-blob-store.md)). Attaching copies no bytes. Identical bytes are
   one stored object however many messages, copies or recipients name them.
2. **The references are Attachment rows.** [ADR-121](ADR-121-attachments-first-class.md) defines the
   `attachments` table on the canonical main backend. It is keyed by `(record_uuid, role)`, carries
   `content_ref`, `media_type`, `size_bytes` and `created_at`, and is a source of liveness for blob
   garbage collection. Rows retained in `attachment_quarantine` also keep their referenced objects
   live until an administrative sweep removes those rows. Each attachment of a message is one row
   on the Note substrate, owned by the message note. The note's `properties` gain no attachment key,
   because a property that names a blob does not keep the blob from being reclaimed (see
   [Retention and garbage collection](#retention-and-garbage-collection)).
3. **Roles are positional.** The table holds one row per role per record. The reference at
   zero-based position `n` of the caller's list is therefore stored under the role
   `message-attachment:n`, for `n` from 0 to 7. The role records a position and says nothing about
   format. It is distinct from the `quarantine-original` role that channel quarantine uses. A
   reader selects exactly these eight role names on Note rows and orders them by `n`. Any other
   role on a message note is not an attachment of that message, including a role such as
   `message-attachment:8`.
4. **What a row records.** `content_ref` is the reference the caller supplied. `size_bytes` is the
   object's size as the blob store reported it when the message was sent. `media_type` is null in
   this version, because the blob store keeps no media type and the send carries none. No file
   name is recorded.
5. **Both copies carry the rows.** A send writes an outbound copy for the sender and an inbound
   copy for the recipient ([ADR-057](ADR-057-comm-actor-addressed-delivery.md)). Each copy has its
   own complete set of rows with the same roles, references and sizes, so deleting one copy leaves
   the other complete.
6. **The rows are fixed at send time.** This version has no verb that adds, replaces or removes an
   attachment of a message that has been sent.
7. **Relation to the note boundary in ADR-121.** ADR-121 §2 limits a note attachment to the note's
   own content in another modality, and places an independent thing, such as a report delivered
   through a conversation, in an entity that the message annotates. This amendment lets a sender
   attach any file to a message, and the rows record what the message carried. A recipient who
   wants a file as a named record creates the entity and attaches the same content reference, as
   ADR-121 §7 describes, with no byte copy and no change to the message. The positional roles are a
   use of the existing table and do not extend ADR-121's rule that a role names a rendition.

### Verb changes

#### `comm.send` and `comm.reply`

Both verbs accept an optional `attachments` array of at most eight distinct content references.
Each reference is the 64-character lowercase hexadecimal string that `blob.put`, `blob.commit` or
`blob.import` returns. The list keeps the caller's order, and the views show it in that order. An
empty list is the same as omitting the parameter: it needs no blob store and no particular backend,
and it leaves the request identity unchanged.

A call with a non-empty list runs these checks in order before any write and returns the first
failure:

1. The list has more than eight references.
2. The recipient is an outbound channel address (see [External channels](#external-channels)).
3. Comm is served by a backend other than the canonical main backend (see
   [Placement and the transaction](#placement-and-the-transaction)).
4. The server has no blob store installed. This failure is the existing unconfigured error.
5. For each reference in list order, one of these holds: the value is not a valid content
   reference, it repeats an earlier reference in the list, no object is stored under it, or adding
   its stored size takes the running total above 64 MiB (67,108,864 bytes).

Every failure except the fourth is an invalid-input error whose message names the offending
reference, or the recipient or limit involved. A missing object refuses the whole call and names
that reference. A refused call writes no note and no attachment row. This amendment adds no error
code, so the wire form of these errors is whatever the request surface gives any invalid-input
error.

`comm.reply` takes the same list, and the list belongs to the reply alone. A reply does not copy the
attachments of the message it answers. The recipient that check 2 examines is the reply's resolved
other party, which is a channel address when the answered message was ingested from a channel.

**One transaction.** The two note rows, their index rows, and every attachment row of both copies
commit in the single transaction that already makes the dual write atomic. If any statement fails,
including an attachment insert, neither note exists afterwards. No attachment row is written after
that transaction commits.

**Caller-keyed requests.** With an `idempotency_key` and a non-empty list, the ordered list is part
of the request identity. The same key with a different list, or with the same references in a
different order, is the existing `key_conflict`. The same key with the same list returns the
original result without writing, once both stored copies are found to carry exactly the expected
rows: the same roles, references, sizes and media types. A pair that lacks a row is a
`key_conflict` and is not repaired. A request without attachments keeps the identity it had before
this amendment, so keys minted earlier remain valid. The list is validated on every attempt,
including a replay, so a replay that names an object no longer in the store is refused at
validation.

#### `comm.inbox`, `comm.thread` and `comm.read`

Each message record in an `inbox` or `thread` result gains `attachments`, an array of
`{content_ref, size, media_type}` objects in list order. `size` is the recorded `size_bytes`, and
`media_type` is null in this version. A message without attachments carries an empty array in the
canonical payload. The agent presentation drops that empty array, as [ADR-045](ADR-045-verb-response-presentation.md)
specifies for every empty array. `attachments` joins the closed `fields` vocabulary of both verbs
and is assembled before projection, so `fields=["attachments"]` returns it and the other
projection rules are unchanged. With `box="sent"`, `comm.inbox` shows the rows of the outbound
copy. A thread shows one entry for the two copies of a message, as it does today, and because both
copies carry the same rows that entry shows the same attachments whichever copy it is built from.

`comm.read` adds the same array to the message fields that the 2026-09-24 amendment returns for a
successful mark. With `body=false` it adds nothing and keeps its acknowledgement-only shape.

An unreadable ownership row does not hide the message or its readable file references. These
views add `attachments_error: {count, reason: "unreadable_attachment"}` to the affected message
when its attachment-store report contains unreadable rows. `count` is the number of unreadable
rows in `attachments` plus retained rows in `attachment_quarantine` owned by that message UUID.
This diagnostic is owner-wide: it is counted before the eight positional display roles are
selected, so unreadable rows with other roles, including `quarantine-original`, can contribute.
Readable rows with other roles are neither displayed nor counted as errors. The count describes
metadata rows, not missing blob bytes, and no role, raw invalid reference or file bytes are echoed.
A clean message omits the marker. `attachments_error` is an accepted `inbox` and `thread`
projection field; an absent marker projects as null when explicitly selected. A body-bearing
`comm.read` returns the same diagnostic, while `body=false` returns neither attachment field.
A failure of the attachment lookup itself fails the verb rather than producing this marker;
bulk body-bearing reads complete those lookups before marking any target read.

No comm response contains file bytes. Selection, ordering, pagination, counts, deduplication,
cursors and read state are unchanged. `comm.delivered`, `comm.mark_read`, `comm.unread`,
`comm.health`, `comm.probe`, `comm.ingest` and the generic record verbs are unchanged.

#### Moving bytes in and out

A sender stores an object with `blob.put`, with the staged `blob.begin`, `blob.put_part` and
`blob.commit` sequence, or with `blob.import`, and passes the returned reference to `comm.send`. A
recipient reads the references from its message views, then reads an object with `blob.get` or
writes it to the server's disk with `blob.export`. `blob.get` is unchanged. Its response is bounded
by the daemon frame, so an object larger than one frame is read by successive calls with `range`
([ADR-173](ADR-173-blob-chunked-upload.md) describes the bound).

#### `blob.import` and `blob.export`

`blob.import(path, media_type?)` reads one regular file beneath the import directory and stores it
in the blob store. Its result is `{content_ref, size}`, plus the `media_type` argument echoed when
it was supplied. The echo is a receipt, and the blob store records no media type. The file is at
most 64 MiB. It passes through the existing staged-upload path in 64 KiB parts, so the reference is
the BLAKE3 digest of the bytes the store accepted, the declared length is checked at commit, and
the staged-upload ceilings and idle expiry apply. A file whose length changes during the read is
refused. A blob store without staged-upload support refuses the call, and there is no whole-file
buffering fallback.

`blob.export(content_ref, path)` verifies and reads an existing object of at most 64 MiB through
the runtime's shared admission control, writes it to a temporary file in the destination's
directory, and renames that file over the destination. Its result is `{path, size}`. A failure
before the rename leaves any earlier destination file intact. A caller that loses the response
cannot tell from it whether the file was written.

Neither result contains file bytes. Both verbs are classified as write operations in the gate's
operation table ([ADR-129](ADR-129-fail-closed-gate-default.md) Amendment 3), so a read-only
runtime refuses them and a `deny_writes_for` restriction denies them. The table's classifier
revision changes with this addition.

### Limits

| Limit                                        | Value                                 | Enforced by                                                                                           |
| -------------------------------------------- | ------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| References in one send or reply              | 8, no duplicates                      | the comm handlers, before any write (checks 1 and 5)                                                  |
| Total stored size of those references        | 64 MiB                                | the comm handlers, from the sizes the blob store reports (check 5)                                    |
| Size of one stored object                    | 64 MiB                                | `blob.put`, `blob.begin` and `blob.import` on the way in; `blob.get` and `blob.export` on the way out |
| Decoded size of one `blob.put_part` argument | the `part_limit` `blob.begin` returns | `blob.put_part`                                                                                       |

The count and the total bound one call's list, and both copies of the message share that list. The
values are fixed in this version, and changing them needs an amendment. They are limits on a
message and set no quota on an actor or a mailbox.

### Confined roots for file import and export

`blob.import` and `blob.export` read and write the filesystem of the machine that runs the khive
server. Each verb is confined to one directory.

- The import directory is `~/.khive/imports`, or the value of `KHIVE_IMPORT_FROM_ROOT` when that is
  set and non-empty. The export directory is the existing `save_to` directory: `~/.khive/exports`,
  or the value of `KHIVE_SAVE_TO_ROOT`. Each call resolves both directories to canonical paths and
  creates a missing one. A call is refused when the two are equal or when one lies inside the
  other.
- Import accepts a path relative to the import directory, or an absolute path spelled beneath the
  canonical import directory. It refuses an empty path, any `..` component, a file whose canonical
  path lies outside the directory, a symlink at any component below the directory whether it
  points inside or outside, and anything that is not a regular file. It opens the file without
  following links where the platform supports that, and it checks the opened object again.
- Export applies the `save_to` destination policy. A relative path resolves under the export
  directory. The destination is refused when it contains `..`, when its resolved parent lies
  outside the export directory (checked before any directory is created), or when it is an
  existing symlink or directory. An existing regular file is replaced.
- The `blob.export` help names the export directory and `KHIVE_SAVE_TO_ROOT`. On a local server a
  caller that needs the file where it works points that variable at its working directory, so a
  confined export is not mistaken for a failed download.
- Every call is confined. These verbs have no unrestricted operator mode of the kind the
  command-line form of `save_to` has.
- The checks do not stop another local process from swapping a directory between the check and the
  use. An operator configures the two directories and their ancestors so that only trusted
  processes can change them.
- Both verbs are disabled by default. A local deployment opts in with `[blob] file_transfers = true`
  in configuration or `KHIVE_FILE_TRANSFERS=1` in the server environment. The setting is resolved
  once at host boot when runtime configuration is constructed; pack initialization uses that
  resolved snapshot. Without opt-in, either verb refuses and names the setting.
  A deployment that serves callers it does not trust keeps both verbs disabled.

ADR-121 §3 accepts a local file path only on local stdio deployments and rejects it elsewhere with
an error that names the constraint. Explicit opt-in is the blob pack's signal that an operator has
enabled these server-local file transfers. It does not relax confinement or the read-only refusal.
`blob.put` remains available without that opt-in and still accepts base64 bytes only, never a path.

### External channels

Version 1 refuses attachments for any recipient that routes to an outbound transport. The prefixes
`email:`, `telegram:` and `khive1:` (the node address form of
[ADR-105](ADR-105-cross-node-comm-transport.md)) mark those recipients. A `comm.send` or
`comm.reply` to one of them with a non-empty list is refused at check 2. The message text is not
sent without its attachments, so no transport silently drops a file.

Three facts support the rule. [ADR-056](ADR-056-channel-transport-layer.md) limits the Telegram
adapter to text, [ADR-122](ADR-122-email-outbound-delivery.md) does not define attachment delivery
for email, and the node wire protocol of ADR-105 carries no attachments in version 1. A content
reference is a bearer capability (ADR-111 Amendment 4), and sending one over a transport hands it
outside the deployment. A refusal reaches the sender, and a silent drop would not.

A transport added later inherits the refusal. The change that adds its address prefix to outbound
routing adds the prefix to this refusal set, and a later record says how attachments cross it, if
they do.

### Visibility

Mailbox scoping decides who sees a reference. Both copies of a message are in the sender's
namespace (ADR-057), and visibility is the existing actor-addressed rule. A mailbox view shows an
actor's inbound copies, where `to_actor` is that actor, and its outbound copies, where `from_actor`
is that actor. The view's actor is the caller. It is another actor only when the gate authorizes a
delegated read through `mailbox_actor`. `comm.read` requires the caller to be the addressee. The
`attachments` array is part of the message record and follows the record, so the references reach
exactly the actors who can read the message. This amendment adds no authorization seam, and the
legacy-row rules of ADR-057 are unchanged.

The object is not scoped by mailbox. Under [ADR-111](ADR-111-blob-store.md) Amendment 4, possession
of a content reference is the capability to read the object, and `blob.get`, `blob.stat` and
`blob.export` consult neither attachment rows nor message visibility. An attachment therefore
delivers a reference to the recipient. It grants nothing and restricts nothing, and any other
holder of the reference can read the bytes with the same verbs. A sender who needs the bytes to stay
private to the recipients encrypts them before upload, as that amendment advises for sensitive
low-entropy content. Its rule that references stay off public and unauthenticated surfaces applies
to the references in message records.

The existence check in `comm.send` and `comm.reply` is an existence probe of the same kind as
`blob.stat`. Single-user local deployments accept that residue, as ADR-111 Amendment 4 records.
Per-tenant read control is out of scope here (see below).

### Retention and garbage collection

Attachment rows are what keep an attached object alive. The blob sweep treats an object as live
when at least one row in the main database's `attachments` or `attachment_quarantine` table names
its reference (ADR-121 §5, ADR-111 §8). A quarantined row continues to pin its object after hard
deletion of the original record; an administrative sweep retires that ownership by removing the
quarantine row. This ownership accounting does not change the sweep's admission rules.

- **Rows belong to copies.** Each copy of a message owns its rows. Deleting the sender's copy
  releases only the sender's rows, and the recipient's rows keep the object alive.
- **Soft delete keeps the rows and hard delete removes them.** A soft delete leaves a note's
  attachment rows in place. A hard delete removes them in the same transaction that removes the
  note (ADR-121 §6). Neither touches the object's bytes.
- **Reclamation belongs to the sweep.** An object whose last row is gone becomes an orphan, and the
  sweep reclaims it under the rules of ADR-111 §8 and ADR-121 Amendment 1, including the publish
  grace. An object that another record also names, such as an entity or another message with the
  same bytes, stays live. Messages and their attachments have no expiry of their own.
- **Before the send.** An uploaded object that is not yet attached has no row. Only the store's
  publish grace protects it: one hour by default on the filesystem store, restarted when the same
  bytes are put again. The existence check refuses an object that was reclaimed first, so a
  reclaimed object is reported and never attached. The S3 store has no transactional sweep
  (ADR-111 §8).
- **Between the check and the commit.** The check reserves nothing. A sweep that claims the
  reference after the check makes the attachment insert abort, and that abort rolls back the whole
  send. A send that commits first removes the object from the sweep's candidates. A sweep that
  claims, deletes and finishes between the check and the commit is not detected, and the message
  then names a missing object. That needs an object that was last published longer ago than the
  publish grace. A physical deletion outside the sweep, which ADR-111 §8 reserves for offline
  maintenance, has the same effect. This amendment adds no repair for either case.

### Placement and the transaction

Attachment rows exist on the canonical main backend only (ADR-121), and a runtime bound to any
other backend refuses to write them. Note rows live on the backend that serves comm. One
transaction cannot span two databases, and rows written after the notes would leave a window in
which a message exists without its attachments. So when comm is served by a backend other than
main, `comm.send` and `comm.reply` refuse a non-empty list before any write. Messages without
attachments are unaffected on such a backend, and their views return an empty `attachments` array
without reading another database.

The blob store is a separate resource. Object bytes live in the filesystem or S3 store that the
runtime has installed, and no SQL transaction covers them under any routing of the blob pack. The
existence and size check in check 5 is therefore a pre-check that reserves nothing, and
[Retention and garbage collection](#retention-and-garbage-collection) states the window that
leaves.

### Out of scope

- Remote transfer: short-lived signed HTTPS URLs returned over MCP for upload and download, and
  cross-tenant relay of messages with attachments. Both belong to a later amendment of
  [ADR-105](ADR-105-cross-node-comm-transport.md).
- Per-tenant read control over attached references, including applying the put-ledger of ADR-111
  Amendment 4 to the `attachments` argument and any grant of read rights to a recipient. Until it
  exists, comm file attachments suit deployments where the callers trust one another to the degree
  that amendment accepts.
- Attachments over any external channel in either direction, including turning an inbound channel
  attachment into a message attachment. The `quarantine-original` role is unchanged.
- File names and media types. This version records no name and a null media type.
- Adding, replacing or removing the attachments of a sent message.
- Forwarding. [ADR-123](ADR-123-comm-forward.md) is proposed, and its payload lists attachments by
  `role` and `content_ref`. It is reconciled with the view defined here when ADR-123 is accepted.
- Quotas per actor or mailbox, and objects larger than 64 MiB.
- Indexing or embedding attachment content.
- Changes to `blob.get`, to the other comm verbs, and to the generic record verbs.

### Acceptance

Acceptance requires these arms, each selecting at least one test and passing:

1. A send with two references leaves both copies with two rows each under `message-attachment:0`
   and `message-attachment:1`. `comm.inbox` (both boxes), `comm.thread` (both orders) and
   `comm.read` with `body=true` show them in order, and `blob.export` of each reference writes the
   original bytes.
2. A reply with an attachment behaves the same, and does not copy the answered message's
   attachments.
3. Each refusal below returns its specific reason and leaves the counts of notes and attachment
   rows unchanged: a missing object, nine references, a duplicate reference, a total over 64 MiB,
   a malformed reference, an `email:`, `telegram:` or `khive1:` recipient, a reply to a message
   ingested from a channel, and a comm backend other than main.
4. A failure injected into the inbound copy's attachment insert leaves neither note and no rows.
5. A keyed send replays with the same list, conflicts with a different list or a different order,
   conflicts when a stored copy lacks a row without repairing it, and keeps the identity of an
   attachment-free request unchanged.
6. An attachment-free message shows `attachments: []`. `fields=["attachments"]` projects it.
   `comm.read` with `body=false` is unchanged. A `quarantine-original` row and a
   `message-attachment:8` row are never shown.
7. A hard delete removes a copy's rows in the same transaction and leaves the other copy's rows. A
   soft delete leaves them. An injected attachment-delete failure rolls the hard delete back.
8. `blob.import` then `blob.export` round-trips byte-equal, and the imported reference equals the
   BLAKE3 digest of the file. These are refused: a `..` path, a path outside the import
   directory, a symlink inside the directory that points outside it, a symlink inside the directory
   that points inside it, a symlinked ancestor, a directory, a file over 64 MiB, a file whose
   length changes during the read, an export onto a symlink or a directory, equal or nested
   directories, a read-only runtime, and a store without staged-upload support.

A mutation must make its named test fail at its own assertion when it removes the existence check,
the import directory check, the import symlink check, the external-channel refusal, or the replay
row check, writes rows on one copy only, or writes rows after the commit.

### Alternatives considered

- **Carry bytes in comm requests and results.** Rejected. Every inbox read would grow with every
  attachment, and the daemon frame bounds the size of any one call (ADR-173).
- **Record the references in a message property.** Rejected. Garbage collection reads the
  `attachments` and `attachment_quarantine` ownership rows rather than message properties, so a
  property would leave the object open to reclamation after the grace period.
- **Change the attachment table to allow several rows per role.** Deferred. Positional roles fit
  the existing table without a layout migration, at the cost of a role that records a position.
  Migration 047 tightens role validation and preserves rejected rows, independently of that
  positional layout. A later amendment can change the layout if a second consumer needs a
  multi-valued role.
- **Send the text and drop the files for external recipients.** Rejected. The loss would be silent,
  and the reference would travel outside the deployment.
- **Write the attachment rows after the notes commit.** Rejected. It leaves a window in which a
  delivered message lacks its attachments, and a failure there has no clean recovery.
- **Let `comm.send` read a server file by path.** Rejected. It would add a second path-reading
  surface. Uploading through the blob verbs keeps one confined surface.
