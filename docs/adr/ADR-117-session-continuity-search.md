# ADR-117: Session Continuity — Cross-Session Search and Remote Ingestion

**Status**: proposed
**Date**: 2026-07-19
**Authors**: khive maintainers
**Depends on**:

- [ADR-007](ADR-007-namespace.md) — Namespace as Attribution-Only Open String (the Gate is the
  single authorization seam this ADR's tenant scoping is enforced at)
- [ADR-013](ADR-013-note-kind-taxonomy.md) — Note Kind Taxonomy (the `session` note kind the
  session pack registers)
- [ADR-021](ADR-021-memory-pack.md) — Memory Pack (the hybrid FTS + vector recall with RRF fusion
  this ADR reuses rather than re-implements)
- [ADR-025](ADR-025-verb-speech-acts.md) — Verb Speech Acts (the Assertive category
  `session.search` joins)
- [ADR-083](ADR-083-session-pack-t1-verbs.md) — Session Pack T1 Verb Surface (the existing
  `store`/`list`/`resume`/`export` verbs and mirror this ADR extends)

---

## Context

The session pack (`khive-pack-session`) already persists agent-session transcripts. A background
mirror service tails Claude Code, Codex CLI, and ChatGPT export files, parses each line into
structured messages, and writes three tables: `sessions` (one row per transcript, carrying
`provider_session_id`, source, cwd, git_branch, and timestamps), `session_messages` (the full
message text plus the raw line), and `session_mirror_cursor` (a byte-offset cursor that makes the
tail resumable and idempotent). Four verbs read and write this data: `session.store`,
`session.list`, `session.resume`, `session.export`.

There is one capability the pack does not have: **search over message content**. `session.resume`
fetches a single session by id; `session.list` is a recency-ordered browse. Neither can answer
"which session was I working on the retrieval-fusion bug" or "find the session where I first
sketched the blob cache." The message text is stored — it is simply not indexed for retrieval.

Two properties of the existing design make this a build-out rather than a new subsystem:

1. The mirror already ingests from **multiple sources** and is cursor-resumable, so extending
   ingestion to span multiple originating machines is a change of sink and source, not a rewrite.
2. The memory pack already implements exactly the retrieval shape session search needs — hybrid
   FTS5 + vector with reciprocal-rank fusion — over a different table. Session search is that same
   shape over `session_messages`.

This ADR specifies the search capability, the ingestion model that lets a single logical account
span multiple machines, the isolation contract that keeps one account's transcripts private in a
multi-tenant deployment, and the retention and deletion model for what is the most sensitive data
the substrate holds.

This ADR specifies the capability and its contract. It does not schedule the implementation.

---

## Decision

### D1 — A `session.search` verb, FTS5 first

Add one Assertive verb to the session pack:

```
session.search(query, limit?, since?, source?, cwd?)
  -> [{ session, score, snippets }]
```

It runs keyword retrieval over `session_messages.text`, scoped to the caller's namespace, and
returns matching sessions ranked by score with the matching message snippets. The optional
filters narrow by recency (`since`), originating tool (`source`), and working directory (`cwd`).

Version 1 is **FTS5-only**. Session-search queries are keyword-rich by nature — a user searching
their own history remembers the terms they typed — and full-text retrieval covers that query
class directly. FTS5 over `session_messages.text` needs an index and a triggered content table,
following the same pattern the note substrate already uses.

The verb's ranking and fusion primitives are shared with the memory pack (ADR-021), not
re-implemented. When vector retrieval is added (D2), `session.search` gains a second signal fused
by RRF exactly as `memory.recall` does; the verb signature does not change.

### D2 — Schema is embedding-ready; vector search is a gated later addition

Version 1 does not embed messages, but the schema is authored so that adding a vector signal later
is a backfill, not a migration of live data:

- `session_messages` rows are **content-addressed** (a stable hash of the message content), so a
  later embedding-backfill job is idempotent and re-runnable without duplicating vectors or
  re-embedding unchanged rows.
- The vector addition is **gated on the per-engine retrieval parallelization work**
  (khive#1121). Embedding every message means an embedding cost that scales with the size of a
  user's history rather than their current activity, and vector recall rides the same serialized
  per-engine retrieval path #1121 addresses. Adding a second retrieval signal before that path is
  parallel would compound a known latency cost. This is the same sequencing constraint the
  multi-engine default change applies (khive#1116): the more expensive retrieval mode does not
  ship until the path that makes it affordable does.

Until vector search ships, the capability is described in every surface as **full-text search
across sessions**, never semantic search — the contract must not advertise a signal that is not
yet in the retrieval path.

### D3 — Ingestion: the existing mirror with a remote sink

Spanning multiple originating machines under one logical account does not require a new client
tool. The existing mirror is extended with a **remote sink**: instead of writing to a local
table, it streams parsed messages to a per-account ingestion endpoint. The endpoint is
admin/bulk-shaped — a streaming ingestion path, not a per-message interactive verb round-trip —
consistent with keeping long-running bulk work off the interactive verb surface.

The byte-offset cursor already resolves what would otherwise be two separate code paths. A fresh
machine's cursor starts at zero, so the same tail loop performs a cold bulk backfill of that
machine's existing transcripts and then continues into live tailing, with no separate uploader.
A one-shot upload of an archived transcript directory fast-follows against the same endpoint.

Re-streaming is idempotent by construction: `provider_session_id` identifies the logical session
and the content-addressed message rows (D2) dedup individual messages, so a machine re-syncing, or
the same transcript arriving from a backup, never doubles a session.

### D4 — Tenant scoping is Gate-enforced and proven, in the same change (hard requirement)

Session transcripts contain everything a user typed and everything they were shown. In a
multi-tenant deployment, one account's sessions must never be reachable by another account's
query, and per ADR-007 that boundary is enforced at the Gate, not by a convenience filter in the
handler.

Therefore `session.search` lands **only together with** its Gate-enforced tenant scoping, in the
same change, plus an isolation test that constructs a crafted cross-account query and asserts it
returns nothing. The scoping is a property proven by test, not an aspiration documented in prose.
A `session.search` that is not tenant-scoped by construction does not ship, even partially.

This extends the same discipline the substrate already applies to multi-record reads: the default
is the caller's own namespace, and the only widening is an explicit, authorized parameter the Gate
evaluates.

### D5 — Retention bounded by a storage cap, with visible aging and user deletion

Retention is bounded by a configurable **storage cap**, not by a time window. Within the cap,
history is kept indefinitely, so a search can reach back across the full retained span. When the
cap is reached, the account ages out its oldest sessions first, and that aging is **always
explicit and surfaced to the user** — never a silent background deletion.

Independent of the cap, **user-initiated deletion is available from day one**, at two
granularities: a single session, and a full wipe of all session data for the account. Deletion is
a curation-layer operation (ADR-014), not a view-layer filter: the data is removed, not hidden.

---

## Consequences

**Enables.** Content search across an account's entire retained session history, spanning every
machine that streams to it, is the capability D1 through D3 deliver together. None of the three is
useful alone: search without cross-machine ingestion sees one machine; ingestion without search is
storage; either without D4 is unsafe to operate multi-tenant.

**Cost.** The dominant cost is D2's vector mode: embedding every message is compute and storage
that scales with history size. FTS5-only v1 is comparatively cheap and is the reason v1 is the
floor. Retrieval latency for session search is the same shape measured for `memory.recall`, so the
retrieval-path work in khive#1116 and khive#1121 is a direct cost lever on this capability, not an
unrelated lane.

**Sensitivity.** Holding full transcript history is a data-sensitivity obligation as much as a
storage bill. D4's proven isolation and D5's user-initiated deletion are load-bearing for that
reason, not optional polish.

**Surface delta.** One new verb (`session.search`) and one internal ingestion endpoint. The
existing four session verbs are unchanged. The local single-source mirror remains for the
single-machine case; the remote sink is an added ingestion mode, not a replacement.

---

## Alternatives considered

**Vector search in v1.** Rejected for sequencing, not merit: it rides the un-parallelized
per-engine retrieval path and embeds cost that scales with history. Deferred behind khive#1121 and
made a schema-ready backfill (D2) rather than dropped.

**A separate uploader tool for cross-machine ingestion.** Rejected as redundant. The mirror's
zero-cursor cold start already performs bulk backfill through the same loop as live tailing (D3),
so a second tool would duplicate the ingestion and dedup logic.

**A time-window retention bound.** Rejected in favor of a storage cap (D5). A fixed time window
either discards history a search should still reach or grows without bound; a storage cap keeps
history reachable up to a declared limit and makes aging an explicit, surfaced event.

**Namespace-filter tenant scoping in the handler.** Rejected as unsafe. Per ADR-007 the
authorization boundary is the Gate; a handler-level `WHERE namespace=` is a convenience, not a
boundary, and for data this sensitive the isolation must be Gate-enforced and test-proven (D4).
