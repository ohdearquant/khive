# ADR-057: Comm Actor-Addressed Delivery

**Status**: Accepted\
**Date**: 2026-06-15\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-007 (Namespace), ADR-017 (Pack Standard), ADR-040 (Communication and
Schedule Packs)\
**Related issues**: #57 (actor-addressed delivery -- primary), #13 (cross-namespace policy
gate), #75 (actor identity on every request)

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
the write is denied with `PermissionDenied`. Agent-to-agent messaging in the default OSS
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
Section "Cross-namespace messaging" specifies and what issue #13 addresses. It is the correct
multi-tenant path for khive-cloud. It is not the fix for the local single-namespace case.

Option B is deferred. Issue #13 remains open and will be addressed in a future cloud-tier ADR
(likely a companion to ADR-053). This ADR does not conflict with Option B; both can coexist
because the actor-addressed path fires only when sender and recipient share a namespace.

### Scope of this implementation

This PR delivers **Failure 1 (delivery denied)** for the shared-`"local"` deployment:
`comm.send` and `comm.reply` no longer return `PermissionDenied` for actor-addressed sends.
Both copies of a message stay in the caller's namespace; delivery works via the single-actor
fallback in `comm.inbox` (no `to_actor` filter when `caller_actor == "local"`).

**Failure 2 (per-actor inbox filtering)** requires distinguishing actors within a shared
namespace, which in turn requires actor identity to be carried separately from the namespace
string. That work is tracked in issue #75. The `to_actor` field, the
`idx_comm_message_to_actor` index, and the `EqOrMissing` filter in `handle_inbox` are
forward-deployed and dormant: they activate automatically once tokens carry distinct actor
labels within the same namespace.

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

### `comm.inbox` response shape

`comm.inbox` surfaces the following top-level convenience fields on each returned message
object for scannability. The canonical values remain stored in `properties`; these fields are
extracted at view time and are additive (no existing keys are removed or renamed).

| Field       | Source                                                                               | Default when absent |
| ----------- | ------------------------------------------------------------------------------------ | ------------------- |
| `from`      | `properties.from_actor`, fallback to `namespace`                                     | `namespace` value   |
| `to`        | `properties.to_actor`                                                                | null                |
| `subject`   | `properties.subject`                                                                 | null                |
| `read`      | `properties.read`                                                                    | false               |
| `direction` | `properties.direction`                                                               | null                |
| `preview`   | derived: whitespace-collapsed, truncated to 80 chars with `...` appended when longer | (always present)    |

The `preview` field is computed from `content` in the view layer. Stored content is never
mutated. When `subject` is null, `preview` provides a fallback scan line for the inbox.

### `comm.send` behavior change

The `to` parameter is reinterpreted. When `to` does not start with a recognized remote
transport prefix (currently there are none in OSS; ADR-056 channel adapters will introduce
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

The `dual_write_message` function does not need to be rewritten: the case where both copies
land in the caller's namespace already works today (same-namespace send path). The only
changes are that `from` and `to` in properties no longer need to be valid namespace strings,
and two new fields (`from_actor`, `to_actor`) are added to the properties JSON for both
copies.

### `comm.inbox` behavior change

`comm.inbox` adds a `to_actor` filter:

- If the caller's actor label equals `"local"` (the single-actor fallback), no `to_actor`
  filter is applied. The inbox behaves exactly as today: all inbound messages in the namespace
  are returned. This preserves backward compatibility for existing single-actor deployments.
- If the caller's actor label is anything other than `"local"`, an additional property filter
  is applied: `properties.to_actor == caller_actor_label`. Only messages addressed to this
  actor are returned. Messages without a `to_actor` field (legacy messages) are not visible
  to actor-scoped callers; see Open Question Q3.

The `status` filter (`unread`, `read`, `all`) is unchanged.

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

### `comm.thread` and `comm.read` behavior changes

No changes to thread resolution or read-marking logic. Thread queries filter by
`properties.thread_id`, which is namespace-scoped and independent of actor labels. Read
marking is a per-message operation gated on namespace membership, which is unchanged.

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

Messages written before this ADR lack `from_actor` and `to_actor` fields. Because the
single-actor fallback skips the `to_actor` filter when the actor label is `"local"`,
existing deployments that have not configured `--actor` or `[actor] id` see no change in
inbox behavior. Deployments that set an actor label will not see legacy party-line messages
in their actor-scoped inbox; see Open Question Q3.

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

**(a) Per-actor inbox filtering -- deferred to issue #75**

Steps 6-8 of the original plan (inbox as `lambda:leo` sees the message; inbox as `lambda:khive`
does not) require carrying actor identity separately from the namespace string. With the current
mechanism, `token.namespace().as_str()` IS the actor label, so two actors in the same namespace
`"local"` would both have actor label `"local"` and both see all messages via the single-actor
fallback. Per-actor filtering within a shared namespace is therefore not achievable with this
implementation and is deferred to issue #75. The `to_actor` field, the
`idx_comm_message_to_actor` index, and the `EqOrMissing` inbox filter are forward-deployed
machinery that activate once tokens carry distinct non-`"local"` actor labels.

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
8. Assert send succeeds, no `PermissionDenied`.
9. Assert the inbound note is in namespace `"local"` with `to_actor="lambda:leo"`.
10. `comm.inbox` with actor label `"local"`: the inbound message appears (the `caller_actor ==
    "local"` branch skips the `to_actor` filter, returning all inbound messages).
11. Assert `comm.reply` from `"local"` on the inbound note succeeds and all four notes
    (outbound1, inbound1, outbound2, inbound2) remain in namespace `"local"`.

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
of A2. Issue #13 remains valid as the Option B multi-tenant path and should be implemented
separately.

**A5. Wait for #75.** Block actor-addressed delivery on the full actor-identity-on-every-request
implementation. Rejected because the actor label needed for message routing is already available
from `token.namespace().as_str()` in the current code. Issue #75 is a general improvement;
blocking comm on it leaves agent-to-agent messaging broken without benefit.

## Open Questions

The following questions could not be fully resolved from source and require Ocean's judgment
before implementation begins.

**Q1. Actor label validation strictness.** This ADR proposes that `to` actor labels be
validated for non-empty, no control characters, and max 255 bytes, but not via
`Namespace::parse`. If Ocean prefers that actor labels be required to be valid namespace
strings, the send handler can call `Namespace::parse(to)` and return an error for
non-conforming values. The tradeoff: strict validation improves type safety but rejects labels
that future transport adapters (ADR-056) may need to express (e.g., email addresses or channel
identifiers as actor labels in `comm.send`). Decision needed before implementation.

**Q2. Index creation placement.** The new `idx_comm_message_to_actor` index is proposed to be
added via `COMM_SCHEMA_PLAN_STMTS` (run idempotently at pack startup via `CREATE INDEX IF NOT
EXISTS`). Ocean should confirm this approach is acceptable, or specify that the index belongs
in a numbered `VersionedMigration` (ADR-015) to keep startup behavior predictable.

**Q3. Legacy message visibility.** Messages written before this ADR have no `to_actor` field.
Under the proposed fallback, these messages are visible only to callers whose actor label is
`"local"`. Callers with a configured actor label (e.g., `lambda:leo`) will not see them.
Whether existing party-line messages should be backfilled with `to_actor = "local"` (to
remain visible in single-actor inboxes) or declared out-of-scope for actor-scoped inboxes is
a product decision Ocean must settle before the migration story is finalized.
