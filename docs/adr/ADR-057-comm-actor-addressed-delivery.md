# ADR-057: Comm Actor-Addressed Delivery

**Status**: Accepted (amended 2026-08-06 — named atomic mark-read; 2026-08-09 — inbox
limit-clamp disclosure)\
**Date**: 2026-06-15 (amended 2026-08-06 and 2026-08-09)\
**Authors**: khive maintainers
**Depends on**: ADR-007 (Namespace), ADR-017 (Pack Standard), ADR-040 (Communication and
Schedule Packs)\
**Related issues**: #57 (actor-addressed delivery -- primary), #13 (cross-namespace policy
gate), #75 (actor identity on every request), #1447 (sender-side dual-write confirmation),
#1428 (process provenance), #1490 (versioned message properties), #1468 (list-read field
projection), #1471 (sender-visible sent history), #199 (anonymous inbox isolation),
and #1387 (named atomic mark-read), plus #1761 (inbox limit-clamp disclosure)

## Context

The comm pack (`khive-pack-comm`) was designed with a cross-namespace delivery model: `to`
is a namespace string and `dual_write_message` writes the inbound copy into the recipient's
namespace. ADR-040 later permitted same-namespace sends (sender and recipient in the same
namespace) without an allowlist entry. The `from` field in stored message properties is set
to `token.namespace().as_str()` (handlers.rs:50, 278).

In practice, all local MCP sessions launch as `kkernel mcp` with no `--actor` flag and no
`khive.toml` with an `[actor] id` entry. The runtime falls back to namespace `"local"`. Every
lambda in the same deployment therefore shares namespace `"local"`.

This creates two concrete failures documented in issue #57:

**Failure 1 -- delivery denied.** `comm.send(to="lambda:leo")` resolves `to` as a namespace
string. Because `"lambda:leo" != "local"`, `dual_write_message` attempts a cross-namespace
write. The sender's `actor.allowed_outbound_namespaces` is empty by default (ADR-040), so
the write is denied with `PermissionDenied`. Agent-to-agent messaging in the default
deployment is non-functional.

**Failure 2 -- party-line inbox.** When senders work around Failure 1 by injecting routing
information into subject prefixes, or when same-namespace sends do succeed, `comm.inbox`
returns all inbound messages in the caller's namespace with no addressee filter
(handlers.rs:105-128). Every lambda sees every other lambda's mail.

Issue #13 proposes routing the cross-namespace check through the AllowAll gate. This resolves
Failure 1 for deployments where agents in different namespaces need to communicate. However,
it does not resolve either failure in the local shared-namespace case: the inbound copy would
land in the recipient's namespace, which nobody reads (all agents are in `"local"`), and the
party-line inbox persists regardless.

Issue #75 proposes that every request carry an authenticated actor identity so that verbs can
scope reads and writes by actor. That is the correct long-term model. However, #75 requires
changes to the dispatch layer, gate stack, and several packs. Gating comm actor-addressed
delivery on #75 would leave agent-to-agent messaging broken for an extended period.

### Namespace is attribution, not isolation (ADR-007 Rev 3)

ADR-007 Rev 3 (2026-06-17, Accepted/Ratified) establishes that namespace is attribution-only:
a write-stamp on records, queryable and filterable, available to the Gate as policy input, but
not a storage boundary. Isolation is enforced at one seam — the Gate (ADR-018, ADR-053) — not
in storage partitions or by-ID namespace checks. The local shared-namespace deployment
intentionally places all lambdas in `"local"` so that memory and KG records are cross-visible.
Per-lambda namespaces would orphan the existing corpus from every lambda's view. Actor identity
and data visibility are orthogonal axes; conflating them by creating per-lambda namespaces is
the wrong fix.

## Decision

### Option A (this ADR): actor-addressed delivery within a namespace

`to` in `comm.send` is reinterpreted as an actor label when the sender and recipient share a
namespace. The actor label is resolved against the caller's own deployment context; no
cross-namespace write occurs. Both the outbound and inbound copies remain in the caller's
namespace. `comm.inbox` is filtered by the caller's actor identity.

This is an additive change: single-actor deployments (no `--actor` / no `KHIVE_ACTOR`) are
backward-compatible because the actor label falls back to the namespace string, preserving
existing behavior.

### Option B (future): cross-namespace ACL delivery

`to` names a namespace; the recipient namespace declares accepted senders; `dual_write_message`
mints a recipient-scoped token via `NamespaceToken::with_namespace`. This is the design ADR-040
Section "Cross-namespace messaging" specifies and what issue #13 addresses. It is the design
path for multi-actor deployments. It is not the fix for the local single-namespace case.

Option B is deferred. Issue #13 remains open for a future ADR covering multi-actor
actor-addressed delivery (likely a companion to ADR-053). This ADR does not conflict with
Option B; both can coexist because the actor-addressed path fires only when sender and
recipient share a namespace.

### Scope of this implementation

The original implementation delivered **Failure 1 (delivery denied)** for the shared-`"local"`
deployment: `comm.send` and `comm.reply` no longer return `PermissionDenied` for
actor-addressed sends, and both copies stay in the caller's namespace.

Subsequent actor propagation and issue #199 completed **Failure 2 (per-actor inbox
filtering)**. `handle_inbox` now applies `to_actor = caller OR to_actor IS NULL` for every
caller, including `"local"`. Anonymous sessions share the `"local"` mailbox and retain access
to legacy rows without `to_actor`, but do not see messages explicitly addressed to another
actor. The `to_actor` field, `idx_comm_message_to_actor` index, and `EqOrMissing` filter are
therefore active compatibility and isolation machinery, not a dormant future path.

`comm.reply` is fail-closed: it always writes both copies into the caller's namespace and
always sets `from_actor`/`to_actor`. No code path through `handle_reply` can cause
`dual_write_message` to mint a token in a foreign namespace.

### Interaction with issue #75

Issue #75 (actor identity on every request) is not a hard prerequisite for this ADR. The
reason is grounded in the current code: `kkernel mcp` already accepts `--actor` / `KHIVE_ACTOR`
(args.rs:29) and `actor.id` in `khive.toml` (engine_config.rs:104), which set the runtime's
`default_namespace` (engine_config.rs:155, config.rs:396-404). The `NamespaceToken` already
carries an `ActorRef` (config.rs:77, 158). The actor label for message routing can therefore
be extracted from the token's actor reference or, for the common fallback case, from the
namespace string at dispatch time.

What this ADR requires of the comm pack is narrowly scoped: read the actor label from the
token and store it on message properties as `from_actor` and `to_actor`. This does not depend
on #75's broader goal of per-verb read scoping across all packs. Issue #75 is the general
follow-up; this ADR delivers the comm-specific case without waiting for the full
actor-identity overhaul.

## Design

### Actor label resolution

The actor label for a session is resolved in the following order (highest wins):

1. CLI `--actor` or env `KHIVE_ACTOR`: the value is parsed as a `Namespace` string and becomes
   `default_namespace`. The `NamespaceToken` carries `ActorRef::anonymous()` at this layer
   today (config.rs:323, 376). The actor label exposed by the comm pack is
   `token.namespace().as_str()`.
2. `[actor] id` in `khive.toml`: same mechanism as (1); the resolved namespace string is the
   actor label.
3. Fallback: `"local"`. In this case a single-actor deployment behaves exactly as today;
   actor-addressed routing degenerates to same-namespace routing.

Because `ActorRef` in the token is currently always `anonymous` (config.rs:323), the comm
pack derives the actor label from `token.namespace().as_str()`. This is the identity the
`[actor] id` config knob already controls. When #75 lands and tokens carry a non-anonymous
`ActorRef`, the comm pack can switch to `token.actor().id` for finer granularity without any
schema change to stored messages.

### Message schema changes

Two fields are added to message note `properties`. Both are optional and default to the
namespace string when absent, preserving backward compatibility with messages written before
this ADR.

| Field        | Type   | When set                              | Value                                                   |
| ------------ | ------ | ------------------------------------- | ------------------------------------------------------- |
| `from_actor` | string | On every `comm.send` and `comm.reply` | Actor label of the sender: `token.namespace().as_str()` |
| `to_actor`   | string | On every `comm.send` and `comm.reply` | The `to` argument as supplied by the caller             |

Existing messages that lack these fields are treated as if `from_actor == namespace` and
`to_actor == "local"` (the single-actor fallback). No database migration is required; these
are JSON properties, not columns.

#### Amendment: versioned properties and process provenance (2026-07-31)

The message-note `properties` object is a stable, versioned reader contract. Every successfully
completed message write by `comm.send`, `comm.reply`, or `comm.ingest` carries integer
`comm_schema_version = 1`. A missing marker identifies the pre-versioning layout; it does not
implicitly mean v1. Any later change to the presence, type, or meaning of a stable field, or an
addition to the stable-field table, requires a version bump. Existing rows are not rewritten.

A root send generates its canonical thread UUID before either note is written, so the outbound
and inbound copies both carry the final `thread_id` and `comm_schema_version = 1` from their
first (and only) write. Both notes commit through one atomic two-note transaction
(`khive_runtime::create_notes_atomic`); a failure on either note rolls back the whole unit, so no
unversioned or partial row can be observed.

The normative field table and reader rules live in
[`message-properties.md`](../../crates/khive-pack-comm/docs/api/message-properties.md).

`comm.send` and `comm.reply` also receive optional `KHIVE_PROCESS_REF` provenance resolved by the
originating request process at dispatch time. When present as Unicode, the exact opaque value is
carried through the warm-daemon request context when necessary and stored as `sent_by_process` on
both delivery copies. When absent, that property is omitted; a shared daemon never substitutes its
own environment. It never participates in identity, routing, authorization, visibility,
threading, or deduplication; `from_actor` remains the actor identity. `comm.ingest` does not stamp
process provenance because the adapter process is not the author of the received message.

### `comm.inbox` response shape

`comm.inbox` surfaces the following top-level convenience fields on each returned message
object for scannability. The canonical values remain stored in `properties`; these fields are
extracted at view time and are additive (no existing keys are removed or renamed).

| Field       | Source                                                                             | Default when absent |
| ----------- | ---------------------------------------------------------------------------------- | ------------------- |
| `from`      | `properties.from_actor`, fallback to `namespace`                                   | `namespace` value   |
| `to`        | `properties.to_actor`                                                              | null                |
| `subject`   | `properties.subject`                                                               | null                |
| `read`      | `properties.read`                                                                  | false               |
| `direction` | `properties.direction`                                                             | null                |
| `preview`   | derived: whitespace-collapsed, truncated to 80 chars with `…` appended when longer | (always present)    |

The `preview` field is computed from `content` in the view layer. Stored content is never
mutated. When `subject` is null, `preview` provides a fallback scan line for the inbox.

#### Amendment: sent box and list-read projection (2026-08-01)

`comm.inbox` remains inbound-only when `box` is omitted. The additive
`box="sent"` form selects outbound rows whose `from_actor` is the calling actor,
with an optional exact `to_actor` recipient filter and the same pagination and
time-window machinery as the inbox. Attributed callers do not inherit legacy
rows with no `from_actor`; the `local` single-actor fallback does. Read-status
and sender filters remain inbox-only and fail when combined with the sent box.

`comm.inbox` and `comm.thread` accept the same closed `fields` list. The
projection is applied after actor visibility, filtering, pagination, and thread
deduplication. It can select ordinary top-level view fields or stable message
property aliases such as `from_actor`, `to_actor`, and `sent_at`; absent optional
properties render as null, while `from_actor`/`to_actor` fall back to the full
view's `from`/`to` values. Omitting `fields` preserves the complete
historical response, while an empty list or an unknown field is rejected rather
than silently changing shape.

#### Amendment: limit-clamp disclosure (2026-08-09)

`limit=0` remains the count-only envelope and values from 1 through 200 retain the historical
response shape. A value above 200 is still accepted and clamped, but the envelope adds
`requested_limit`, `effective_limit`, and `limit_clamped: true`. Clients can therefore distinguish
the effective page size from the requested size without turning a previously accepted call into an
error. The same metadata is added after either an immediate query or a long-poll result.

### `comm.send` behavior change

The `to` parameter is reinterpreted. When `to` does not start with a recognized remote
transport prefix (currently there are none in the default build; ADR-056 channel adapters will introduce
prefixes such as `channel:telegram:`), the send is treated as actor-addressed within the
caller's namespace:

1. `to` must be a non-empty string. No `Namespace::parse` validation is applied; actor labels
   are not required to be valid namespace strings. Validation rule: the label must not contain
   control characters and must not exceed 255 bytes.
2. `from_actor` is set to `token.namespace().as_str()`.
3. `to_actor` is set to the `to` argument.
4. Both the outbound copy and the inbound copy are written to the caller's namespace
   (`caller_token` for both). No cross-namespace write occurs. No allowlist check is performed.
5. The `from` and `to` properties on stored notes retain their current values for backward
   compatibility. `from` is the namespace string (as before). `to` is the `to` argument (as
   before, now interpreted as an actor label rather than a namespace string).

#### Amendment: explicit configured-actor self-send opt-in (2026-07-14)

When the resolved `to_actor` equals the configured sender actor, `comm.send` rejects the call
unless the caller passes `self_send=true`. The optional boolean defaults to false. The anonymous
`local` party-line fallback is exempt because it does not identify two distinct configured
principals.

This is an intentional compatibility change: configured callers that previously sent to their
own actor label must add `self_send=true` when the message is genuinely a note to self. Callers
trying to address a distinct parent, orchestrator, or sub-agent must instead configure distinct
actor identities. Under ADR-096 Fork 2, project-scoped actor discovery can otherwise make two
sessions resolve the same actor label; failing loudly prevents that identity collapse from being
mistaken for successful inter-agent delivery.

#### Amendment: sender-side dual-write confirmation (2026-08-01)

`comm.send` and `comm.reply` write an outbound note and an inbound sibling with
`properties.outbound_ref=<outbound UUID>`. Since #1565, both rows, their FTS
documents, and their vector rows commit through `create_notes_atomic` in one
writer transaction. Ordinary prepare and plan failures leave neither copy;
the old durable half-pair and compensating-delete states are no longer part of
the write path.

Atomicity does not by itself prove the outcome to a caller when an accepted
writer request loses its typed reply. The writer-task contract reports that
case as `request_state=side_effects_unknown`: either the complete pair may have
committed or neither copy did. `dual_write_message` pre-generates the outbound
UUID and surfaces this case as a structured `RuntimeError::Khive(KhiveError)`
(`kind=conflict`) whose `details.outbound_id` field carries the full
`outbound_id` as a machine-readable wire value — not embedded in the
free-text `message` field — so an automated caller can read it directly off
the MCP error object.

The comm pack exposes `comm.delivered(id=<full-outbound-uuid>)`, a read-only
Assertive verb. It queries for a live inbound `message` in the caller's
namespace whose `from_actor` matches the caller and whose `outbound_ref`
exactly matches the supplied UUID. The outbound row need not exist. A
successful response explicitly reports `delivered` when one or more siblings
exist and `undelivered` when none exists; an operation error leaves the outcome
uncertain. Content, subject, and timestamps are not correlation keys.

Only an error whose outcome is genuinely unknown is relabeled `ambiguous` and
given the full `outbound_id`; known pre-write and rolled-back errors preserve
their existing classification. Prefixes are not accepted because confirmation
uses the UUID as a correlation key and must not depend on resolving another
record first.

This contract confirms only khive's internal inbound sibling. It does not
claim asynchronous external-transport delivery (for example SMTP), whose
accepted/queued/failed state is a separate concern.

Loss of the complete MCP response is also outside this contract. In that case
the caller receives neither the success result nor the structured ambiguous
error, and therefore does not know the server-generated outbound UUID. Closing
that wider exactly-once gap requires a future caller-supplied correlation or
idempotency key on `comm.send`/`comm.reply`.

The actor-addressed routing rule remains unchanged by #1565: both copies land
in the caller's namespace, now through the atomic write path. For this ADR's
addressing change, `from` and `to` in properties no longer need to be valid
namespace strings, and two fields (`from_actor`, `to_actor`) are present in
the properties JSON for both copies.

### `comm.inbox` behavior change

`comm.inbox` applies one actor predicate for every caller:

`properties.to_actor == caller_actor_label OR properties.to_actor IS NULL`.

- A configured actor sees messages addressed to that actor plus legacy messages without a
  `to_actor` field.
- The anonymous `"local"` fallback sees messages addressed to `"local"` plus the same legacy
  rows. It does not bypass actor filtering, so messages explicitly addressed to another actor
  remain hidden.

The `status` filter (`unread`, `read`, `all`) is unchanged.

The optional `box="sent"` path reverses the actor predicate: it requires
`direction=outbound` and `from_actor=caller`, then optionally filters
`to_actor`. The default remains the actor-scoped inbound behavior above.

The `idx_comm_message_direction` index (vocab.rs:17) covers `(namespace, kind, direction,
read, created_at)`. When actor filtering is active, a separate index covering
`(namespace, kind, to_actor, direction, read, created_at)` is needed for the `to_actor`
property filter to use an index seek rather than a full scan.

### `comm.reply` behavior change

`handle_reply` derives `reply_to` from the original message's properties. The current logic
(handlers.rs:285-291) uses `from` and `to` namespace strings. With this ADR, when the
original message has `from_actor` and `to_actor` properties, those are used for the reply
routing decision instead:

- If the reply caller is the original `from_actor`, route to `to_actor`.
- If the reply caller is the original `to_actor`, route to `from_actor`.
- If the original message lacks `from_actor` / `to_actor` (legacy message), fall back to
  `from` and `to` as before.

`from_actor` and `to_actor` are set on the reply message using the same logic as `comm.send`.

### `comm.thread`, `comm.read`, and `comm.mark_read` behavior changes

Thread queries filter by `properties.thread_id`, which is namespace-scoped and independent of
actor labels. `comm.mark_read` is the canonical bulk mutation name added by ADR-040's 2026-08-06
amendment; `comm.read` remains its released compatibility surface. Both names reuse the same target
validation and live mutation filter. An attributed row is markable only by its `to_actor`; a legacy
row with no `to_actor` retains this ADR's accepted `EqOrMissing` fail-open rule. Atomic mode changes
only the transaction boundary across already-authorized targets, not principal resolution,
namespace behavior, or legacy visibility.

### Interaction with ADR-007 Rev 3 (namespace as attribution)

Option A writes both copies to the caller's namespace. No `NamespaceToken::with_namespace`
is called. No cross-namespace write is attempted. ADR-007 Rev 3 is fully satisfied: namespace
is attribution, storage is dumb, and the Gate is the single enforcement seam. The safety
argument: actor-addressed delivery is a routing abstraction implemented within a single
namespace; it changes the actor label on message properties (`from_actor`, `to_actor`), not
the namespace of stored records. Comm is NO-CARRY per ADR-007 Rev 3 Rule 3.

### Interaction with ADR-040 cross-namespace allowlist

The `actor.allowed_outbound_namespaces` check in `dual_write_message` is reached only when
`from != recipient_ns_str` at the namespace comparison level (message.rs:82). In the
actor-addressed local path, `recipient_ns_str` remains `token.namespace().as_str()`, so
`from == recipient_ns_str` and the allowlist check is never reached. The existing
cross-namespace path (Option B) is not disturbed.

### Back-compat: existing party-line messages

Messages written before this ADR lack `from_actor` and `to_actor` fields. `FilterOp::EqOrMissing`
keeps those rows visible to every caller as a compatibility concession. New attributed rows
remain actor-scoped, and an anonymous `"local"` caller sees new rows only when they are addressed
to `"local"`.

## Implementation Sketch

Files that change in `crates/khive-pack-comm/`:

**`src/params.rs`**: no struct change required. A comment on `SendParams.to` should note that
`to` is now an actor label; the `Namespace::parse` call that existed in the old cross-namespace
path is removed from the local-send code path.

**`src/handlers.rs`**:

- `handle_send`: resolve `from_actor` from `token.namespace().as_str()`. Merge `from_actor`
  and `to_actor` into the `properties` JSON for both copies before passing to
  `dual_write_message`.
- `handle_inbox`: resolve the caller's actor label. When the label is not `"local"`, push a
  `PropertyFilter` on `$.to_actor` before the existing `direction` filter.
- `handle_reply`: read `from_actor` / `to_actor` from original message properties; use them
  for reply routing when present, falling back to `from` / `to` for legacy messages.

**`src/message.rs`**: `dual_write_message` may accept optional `from_actor: Option<&str>` and
`to_actor: Option<&str>` parameters that are merged into the properties JSON for both copies.
Alternatively, callers merge these fields into the properties `Value` before the call.

**`src/vocab.rs`**: add a third schema plan statement:

```sql
CREATE INDEX IF NOT EXISTS idx_comm_message_to_actor
    ON notes(namespace, kind,
             json_extract(properties, '$.to_actor'),
             json_extract(properties, '$.direction'),
             json_extract(properties, '$.read'),
             created_at DESC)
    WHERE deleted_at IS NULL
```

Update the `comm.send` `ParamDef` for `to` to read "Actor label to send to (e.g.
`\"lambda:leo\"`)." to reflect the reinterpretation.

No numbered `VersionedMigration` (ADR-015) is required because `from_actor` and `to_actor` are
JSON properties; index creation is idempotent via `CREATE INDEX IF NOT EXISTS` at pack startup.

## Test Plan

Tests assert the following:

**(a) Per-actor inbox filtering**

1. Send one message addressed to `lambda:leo` and one addressed to `lambda:khive` in the same
   namespace.
2. Assert each configured actor sees only its own attributed message.
3. Assert an anonymous `"local"` caller sees neither attributed message.
4. Assert every caller can still see a legacy row with no `to_actor`.

**(b) Namespace isolation is preserved**

1. As `lambda:leo` (namespace `"lambda:leo"`), call `comm.send(to="lambda:khive", content=...)`.
2. Assert: send returns `ok`, no `PermissionDenied` error.
3. Assert: both the outbound and inbound notes have `namespace = "lambda:leo"`.
4. Assert: the inbound note has `from_actor="lambda:leo"`, `to_actor="lambda:khive"`.
5. Assert: no note exists in namespace `"lambda:khive"` after the sequence.
6. Assert `NamespaceToken::with_namespace` is not called during the actor-addressed send path
   (verify by inspection: `dual_write_message` takes the `from_actor.is_some()` branch, which
   uses `caller_token` directly for the inbound write).

**(c) Single-actor fallback delivers messages (Failure 1 fix)**

7. Call `comm.send(to="lambda:leo")` with actor label `"local"` (no `--actor` configured).
8. Assert send succeeds, no `PermissionDenied`, and the inbound note remains in namespace
   `"local"` with `to_actor="lambda:leo"`.
9. Assert `lambda:leo` can read the message while the anonymous `"local"` inbox cannot.
10. Call `comm.send(to="local")` anonymously and assert the `"local"` inbox sees that message.
11. Assert participant reply and dual-write behavior keep all resulting notes in namespace
    `"local"`.

## Alternatives Considered

**A2. Per-lambda namespaces.** Give each lambda a dedicated namespace (`lambda:leo`,
`lambda:khive`, etc.) so that cross-namespace delivery is the natural path. Rejected because
it orphans the existing shared corpus: KG entities, memory records, and tasks written to
`"local"` become invisible from any lambda's view unless `actor.visible_namespaces` is
configured for every session. The operational burden is high and the migration path for
existing deployments is non-trivial.

**A3. Subject-prefix routing (status quo workaround).** Continue encoding routing information
in subject lines (`[lambda:khive -> lambda:leo]`). Rejected because it is brittle,
unqueryable, not indexed, and imposes parsing overhead on every inbox consumer.

**A4. Implement #13 (AllowAll gate bypass) instead.** Route the cross-namespace check through
the policy gate so that AllowAll mode permits cross-namespace delivery without an allowlist
entry. Rejected as the primary fix because it does not solve the party-line inbox problem and
requires agents to run in distinct namespaces, which returns to the corpus-orphaning problem
of A2. Issue #13 remains open as the multi-actor actor-addressed delivery path.

**A5. Wait for #75.** Block actor-addressed delivery on the full actor-identity-on-every-request
implementation. Rejected because the actor label needed for message routing is already available
from `token.namespace().as_str()` in the current code. Issue #75 is a general improvement;
blocking comm on it leaves agent-to-agent messaging broken without benefit.

## Open Questions

The following questions could not be fully resolved from source and require maintainer judgment
before implementation begins.

**Q1. Actor label validation strictness.** This ADR proposes that `to` actor labels be
validated for non-empty, no control characters, and max 255 bytes, but not via
`Namespace::parse`. If maintainers prefer that actor labels be required to be valid namespace
strings, the send handler can call `Namespace::parse(to)` and return an error for
non-conforming values. The tradeoff: strict validation improves type safety but rejects labels
that future transport adapters (ADR-056) may need to express (e.g., email addresses or channel
identifiers as actor labels in `comm.send`). Decision needed before implementation.

**Q2. Index creation placement.** The new `idx_comm_message_to_actor` index is proposed to be
added via `COMM_SCHEMA_PLAN_STMTS` (run idempotently at pack startup via `CREATE INDEX IF NOT
EXISTS`). Maintainers should confirm this approach is acceptable, or specify that the index belongs
in a numbered `VersionedMigration` (ADR-015) to keep startup behavior predictable.

**Q3. Legacy message visibility (resolved).** Messages written before this ADR have no
`to_actor` field. The implemented `EqOrMissing` predicate leaves those rows visible to every
caller rather than assigning them to `"local"` or hiding them from configured actors. This is
the explicit backward-compatibility boundary; newly attributed rows remain actor-scoped.
