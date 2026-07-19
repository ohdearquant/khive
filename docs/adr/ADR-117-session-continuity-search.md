# ADR-117: Session Continuity — Cross-Session Search and Remote Ingestion

**Status**: proposed
**Date**: 2026-07-19
**Authors**: khive maintainers
**Depends on**:

- [ADR-007](ADR-007-namespace.md) — Namespace as Attribution-Only Open String (Rule 8: a pack with
  a principal-scoped backend carries its own isolation contract; the broker's server-side principal
  scope is the Gate for that backend)
- [ADR-013](ADR-013-note-kind-taxonomy.md) — Note Kind Taxonomy (the `session` note kind the T1 verbs
  read and write)
- [ADR-014](ADR-014-curation-operations.md) — Curation Operations (deletion is a curation-layer
  operation that removes data, not a view-layer filter)
- [ADR-018](ADR-018-authorization-gate.md) — Authorization Gate (the pre-dispatch `Gate::check`
  contract this ADR's enforcement composes with, and whose fail-open default this ADR carves a
  fail-closed verb class out of)
- [ADR-021](ADR-021-memory-pack.md) — Memory Pack (the hybrid FTS + vector recall with RRF fusion
  this ADR reuses rather than re-implements)
- [ADR-025](ADR-025-verb-speech-acts.md) — Verb Speech Acts (the Assertive category `session.search`
  joins)
- [ADR-083](ADR-083-session-pack-t1-verbs.md) — Session Pack T1 Verb Surface (the existing
  `store`/`list`/`resume`/`export` verbs over the `notes` table, and the separate mirror aux tables
  this ADR searches)

---

## Context

The session pack (`khive-pack-session`) persists agent-session data in **two distinct stores**, a
split that [ADR-083](ADR-083-session-pack-t1-verbs.md) makes deliberately:

1. **Caller-authored session notes.** The four T1 verbs (`session.store` / `list` / `resume` /
   `export`) read and write the shared `notes` table. These are records a caller explicitly stores.
2. **Passively-mirrored transcript tables.** A background mirror tails Claude Code, Codex CLI, and
   ChatGPT exports and writes three auxiliary tables — `sessions` (one row per transcript, carrying
   `provider_session_id`, source, cwd, git_branch, timestamps), `session_messages` (the parsed
   message text plus raw line), and `session_mirror_cursor` (a byte-offset cursor). ADR-083 keeps
   these separate from the notes precisely because one is caller-authored and one is passive
   ingestion.

Each mirrored event already carries a **scoped identity**: `ParsedEvent.uuid` is the primary key for
idempotency — the Claude Code top-level `uuid`, or the synthesized `"{session_id}:{byte_offset}"` for
Codex, or the ChatGPT `message.id` (`crates/khive-pack-session/src/mirror/parse.rs`).

The one capability neither store has is **search over message content**. `session.resume` fetches a
single session by id; `session.list` is a recency browse. The transcript text sits in
`session_messages` and is not indexed for retrieval.

This ADR specifies that search capability and the four contracts it cannot ship without: the identity
model for mirrored messages, the ingestion model that lets one account span multiple machines, the
tenant-isolation enforcement that keeps one account's transcripts unreachable by another, and the
deletion and retention model for the most sensitive data the substrate holds.

This ADR specifies the capability and its contracts. It does not schedule the implementation.

---

## Decision

### D1 — A `session.search` verb, FTS5 first

Add one Assertive verb:

```
session.search(query, limit?, since?, source?, cwd?)
  -> [{ session, score, snippets }]
```

It runs keyword retrieval over `session_messages.text`, **scoped to the caller's authenticated
tenant** (D4), and returns matching sessions ranked by score with the matching snippets. The optional
filters narrow by recency (`since`), originating tool (`source`), and working directory (`cwd`).

The search corpus is the mirror's `session_messages` — that is where transcript text lives.
Caller-authored session notes (D-context store 1) are metadata records, not transcript text, and are
out of the search corpus in v1; a returned hit's identity is nonetheless guaranteed to resume and
export (D6).

Version 1 is **FTS5-only**. Session-search queries are keyword-rich by nature — a user searching their
own history remembers the terms they typed — and full-text retrieval covers that query class
directly. FTS5 over `session_messages.text` needs an index and a triggered content table, the same
pattern the note substrate already uses. Ranking and fusion primitives are shared with the memory
pack (ADR-021), not re-implemented; when vector retrieval is added (D2), `session.search` gains a
second RRF-fused signal without a signature change.

### D2 — Message identity is scoped; the content hash is an integrity adjunct

Message identity is the existing **scoped `ParsedEvent.uuid`** (provider / session / event scope),
not a hash of content. This is a hard correctness requirement, not a preference:

- A content-only identity **collides across tenants** — identical message text in two accounts would
  map to one row, leaking or losing data across the isolation boundary.
- A content-only identity **collapses legitimate repeated events** — the same text occurring twice in
  one session (a command retyped, a repeated tool result) is two events, and hashing them to one
  identity silently loses the second.
- It would contradict the mirror's shipped design, where `ParsedEvent.uuid` is already the idempotency
  key (`parse.rs`).

A content hash is still carried, but as a **dedup / integrity adjunct** on the scoped id, never the
id: it detects whether a re-streamed event bearing the same scoped `uuid` carries the same content
(integrity on idempotent re-stream), and it gates the future embedding-backfill so an unchanged event
is not re-embedded. Idempotent backfill (D3) is provided by the scoped id — re-streaming the same
provider/session/event is idempotent by that id — with the hash guarding content integrity.

The vector signal remains a gated later addition: the schema is embedding-ready (a stable per-event id
plus the content hash make a later embed backfill idempotent and re-runnable), and the vector add is
**gated on the per-engine retrieval parallelization work** (khive#1121) for the same reason the
multi-engine default change (khive#1116) is — embedding every message is a cost that scales with
history size, and vector recall rides the same serialized retrieval path #1121 addresses. Until vector
search ships, every surface describes the capability as **full-text search across sessions**, never
semantic search.

### D3 — Ingestion: a principal-scoped backend with an authenticated contract

Spanning multiple originating machines under one account is a **principal-scoped backend** in the
sense of [ADR-007](ADR-007-namespace.md) Rule 8: the pack's own ADR (this one) defines the mechanism,
authentication model, migration path, and failure modes, and the backend's **server-side per-principal
scope is the Gate for that backend** (ADR-007 Rule 8 §3) — it composes with the single-seam
authorization model, it does not sit outside it.

The existing mirror is extended with a **remote sink**: instead of writing a local table, it streams
parsed events to a per-account ingestion endpoint. That endpoint carries a real contract, none of it
optional:

- **Authentication and credential-to-account binding.** Every ingestion connection authenticates a
  principal, and the authenticated principal — not any client-supplied namespace or account string —
  determines which account the events land under. A client cannot assert another account's identity.
- **Transport security.** The endpoint is reachable only over an encrypted transport.
- **Origin verification and replay protection.** Requests are integrity-protected and carry
  anti-replay material so a captured stream cannot be re-injected.
- **Failure modes.** Authentication failure, integrity failure, and backend-unavailable are
  enumerated with defined behavior (reject and surface, never silently accept or silently drop).

The endpoint is admin/bulk-shaped — a streaming ingestion path, not a per-message interactive verb
round-trip — consistent with keeping long-running bulk work off the interactive verb surface.

The byte-offset cursor collapses what would otherwise be two code paths: a fresh machine's cursor
starts at zero, so the same tail loop performs a cold bulk backfill of that machine's existing
transcripts and then continues into live tailing, with no separate uploader tool. Re-streaming is
idempotent by the **scoped event id** (D2) — a machine re-syncing, or the same transcript arriving
from a backup, never doubles an event — with the content hash guarding integrity.

### D4 — Tenant isolation is fail-closed and enforced at a specified seam (hard requirement)

Session transcripts contain everything a user typed and was shown. In a multi-tenant deployment one
account's sessions must be unreachable by another account's query. Getting the enforcement mechanism
right matters more than asserting the property, so this decision specifies the mechanism against the
actual Gate contract.

The [ADR-018](ADR-018-authorization-gate.md) Gate is a **pre-dispatch check over request metadata**
(`actor`, `namespace`, `verb`, `args`, `context`) that returns Allow/Deny. It runs **before** the
handler and therefore never sees matched rows; and by ADR-018's fifth principle it **fails open on
infrastructure errors** (a Gate-infra failure logs a warning and proceeds; only an explicit `Deny`
blocks). A search verb returns rows, so row-level isolation cannot be a property of the Gate's
pre-dispatch decision, and it cannot rest on the Gate failing safely — it does not.

Enforcement is therefore specified as follows:

1. **The authenticated tenant is the scope.** The scope applied to a `session.search` query is the
   caller's **authenticated** tenant (resolved from the authenticated principal / installed
   `TenantGate`), never a caller-supplied `namespace` argument, which a caller could forge.
2. **A mandatory query predicate at the handler seam.** The handler applies that authenticated tenant
   as a non-widenable predicate on the search query, so rows outside the tenant are unreachable. This
   is the enforcement seam — it is where rows exist, downstream of the Gate's authorization.
3. **Fail-closed by construction.** `session.search` **requires a positive authenticated tenant
   scope** to execute. Absent one — from any cause, including an authentication or Gate-infrastructure
   error — the handler refuses and returns nothing. This means ADR-018's fail-open default cannot leak
   session data: fail-open yields no authenticated scope, and no scope yields no results. To codify the
   property at the contract level rather than rely only on the handler, ADR-018 is amended to designate
   a **fail-closed verb class** (a Gate-infrastructure error is a `Deny` for these verbs);
   `session.search` is the first member.
4. **Proven by test, in the same change.** `session.search` lands **only together with** this
   enforcement, plus an isolation test that issues a crafted cross-account query and asserts it returns
   nothing, and a test that asserts the verb refuses when no authenticated scope is present. The
   isolation is a property proven by test, not documented in prose.

This is consistent with ADR-007: namespace is a policy input to the Gate and the authenticated tenant
predicate, never a storage partition; the only difference between a permissive and an isolating
deployment is which Gate/`TenantGate` is installed.

### D5 — Deletion and retention are executable across every derived surface

Retention is bounded by a configurable **storage cap**, not a time window. Within the cap, history is
kept indefinitely, so search reaches back across the full retained span; at the cap the account ages
out its oldest sessions first, and that aging is an **executable, surfaced** operation — never a silent
background deletion.

Independent of the cap, **user-initiated deletion is available from day one** at two granularities — a
single session and a full account wipe — and both are **curation-layer** operations
([ADR-014](ADR-014-curation-operations.md)) that remove data, not view-layer filters that hide it.
Because the mirror data is spread across derived surfaces, the deletion contract enumerates each with
verb-level semantics; deletion is not complete until every one is handled:

- `session_messages` rows and the `sessions` row.
- The FTS5 index (the triggered content table) — removed with the rows, so deleted content is not still
  matchable.
- Future vector rows (when D2's vector signal ships) — removed on the same path.
- `session_mirror_cursor` — a deleted session is **tombstoned** by scoped id so a subsequent re-stream
  of the same source file does not resurrect it.
- Remote copies at the ingestion sink, and any in-flight / retry state for that account.
- Any cached or hydrated search results.

Retention aging is itself an executable oldest-first operation over the same surfaces.

### D6 — A search hit's identity resumes and exports

`session.search` reads the mirror's `sessions` / `session_messages`, while `session.resume` and
`session.export` operate on the T1 note store. A search result therefore returns a session identity —
the mirror's `provider_session_id` (and its `sessions` row key) — and this ADR requires that identity
to be **resolvable by `resume` and `export`**: a hit is not useful if it cannot be reopened or
exported. The bridge is specified as part of this change (either `resume`/`export` accept the mirror
session identity directly, or a defined resolution maps it), so every `session.search` result carries
an identity guaranteed to resume and export. The caller-authored note store and the mirror store stay
distinct per ADR-083; D6 bridges their identities for continuity, it does not merge the schemas.

---

## Consequences

**Enables.** Content search across an account's entire retained session history, spanning every
machine that streams to it, resolvable back to resume/export. None of D1–D3 is useful alone: search
without cross-machine ingestion sees one machine; ingestion without search is storage; either without
D4 is unsafe to operate multi-tenant; search without D6 returns hits that cannot be reopened.

**Cost.** The dominant cost is D2's future vector mode: embedding every message is compute and storage
that scales with history size, which is why v1 is FTS5-only and the vector add is gated on khive#1121.
Retrieval latency is the same shape measured for `memory.recall`, so khive#1116 and khive#1121 are
direct cost levers on this capability.

**Sensitivity.** Holding full transcript history is a data-sensitivity obligation. D3's authenticated
ingestion, D4's fail-closed proven isolation, and D5's executable deletion are load-bearing for that
reason, not optional polish.

**Surface delta.** One new verb (`session.search`); one internal principal-scoped ingestion endpoint;
deletion and retention verbs (D5); one ADR-018 amendment (the fail-closed verb class, D4); and the D6
identity bridge for resume/export. The existing four T1 verbs are unchanged. The local single-source
mirror remains for the single-machine case; the remote sink is an added ingestion mode, not a
replacement.

---

## Alternatives considered

**Content-hash message identity.** Rejected as incorrect (D2): it collides identical messages across
tenants and collapses legitimate repeated events, and it contradicts the mirror's shipped scoped-uuid
idempotency key. The hash is retained as an integrity/dedup adjunct instead.

**Gate-decision row isolation / relying on Gate fail-safety.** Rejected as unenforceable (D4): the
ADR-018 Gate is pre-dispatch, sees no rows, and fails open, so row isolation cannot be a property of
its decision. Enforcement is a mandatory authenticated-tenant predicate at the handler seam, fail-closed
by requiring a positive scope, with an ADR-018 amendment codifying the fail-closed verb class.

**A handler-level namespace filter on a caller-supplied namespace.** Rejected as forgeable and as the
prior v1 anti-pattern (ADR-007): the scope must be the authenticated tenant, not a caller argument.

**A separate uploader tool for cross-machine ingestion.** Rejected as redundant (D3): the mirror's
zero-cursor cold start already performs bulk backfill through the same loop as live tailing.

**A time-window retention bound.** Rejected in favor of a storage cap (D5): a fixed window either
discards history search should still reach or grows unbounded; a cap keeps history reachable up to a
declared limit and makes aging an explicit, surfaced event.

**Searching the caller-authored note store and the mirror as one corpus in v1.** Deferred, not
adopted: ADR-083 keeps the two stores distinct by design. v1 searches the transcript text where the
searchable content is, and D6 bridges the identity so hits resume and export; unifying the corpora is a
later option, not a v1 requirement.
