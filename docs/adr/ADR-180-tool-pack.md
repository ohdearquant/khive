# ADR-180: Tool Pack: Capability Registry, Ontological Discovery and Use Policy

- **Status**: Proposed
- **Date**: 2026-09-08
- **Extends**: [ADR-002](ADR-002-edge-ontology.md) (edge ontology; the registry reuses `project`,
  `concept` and the `implements` edge),
  [ADR-023](ADR-023-declarative-pack-format.md) (pack verb surface, visibility and composition, the file keeping its original name; the handlers below
  are declared there and are wire surfaces)
- **Relates to**: [ADR-007](ADR-007-namespace.md) (registry, policy and grants are namespace scoped),
  [ADR-096](ADR-096-warm-daemon-per-request-identity.md) (per-request identity: the actor a
  decision is evaluated for), the knowledge pack
  (`knowledge.suggest` is the shape agents already call; this record gives tools the same shape)

## Context

An agent runtime that sits on khive has no way to ask the store what it may call. Tool discovery in
today's harnesses is per session and flat: a list of names with schemas, no provenance, no
side-effect class, no policy, and nothing that survives the session. Approval is a prompt to a human
in the loop, answered once and forgotten. Skills, plugins, MCP tools and khive's own verbs are four
lists in four formats.

The knowledge pack already solves the retrieval half for documents: a caller describes a need and
gets ranked, composable hits. Tools differ from documents in three ways that justify a pack rather
than a document type: a tool carries a callable contract, a tool call has a side-effect class that
policy keys on, and tools are best found through what they do (a capability) rather than what they
are called.

## Decision

One pack, `tool`, riding on the kg pack. Nothing new in the entity taxonomy.

**Registry.** A registered object is a `project` entity of `entity_type` `tool`, `skill`, `plugin`
or `verb`, tagged `tool-registry` and its kind, with properties `schema` (the callable contract),
`source` (`mcp:<server>`, `khive:<pack>`, `skills:<dir>`), `side_effect` (`read`, `write`, `egress`,
`irreversible`), `trust` (`first_party`, `marketplace`, `external`) and `registered_at`. A
capability is a `concept` entity of `entity_type` `capability`, tagged `tool-capability`, and a
registered object points at each capability it implements with an `implements` edge. Names are
unique per namespace; re-registering an existing name returns the existing object and links any
new capabilities without changing its properties. `tool.register(name, kind, description, schema, source, side_effect, trust, capabilities, tags)` creates the object; `kind` defaults to `tool`, `side_effect` to `write` and `trust` to `external`.

**Discovery.** `tool.suggest(query, limit, kind, actor)` runs two arms and merges them: hybrid
search over the registry, and hybrid search over capability concepts expanded through their
`implements` edges to the objects that implement them, scored at nine tenths of the concept hit.
Every hit carries `via` (the capabilities it was reached through) and the caller's `decision`
(below), so the answer is at once "what exists" and "what I may call". `tool.describe(tool)` and
`tool.list(kind, limit, offset)` read the registry; `tool.ingest(source, ...)` bulk registers
khive's own loaded verbs (source `khive`, one capability per pack, side effect `read` for
assertive verbs and `write` otherwise) or an MCP `tools/list` payload (source `mcp`, `server`,
`tools`).

**Policy.** `tool.policy(actor, tool, decision, note)` stores a rule; `actor` and `tool` are exact
labels, trailing-`*` prefixes or `*`. `tool.check(tool, actor)` resolves in this order: an active
grant for that actor and tool name gives `allow`; else the most specific matching policy wins,
ties broken `deny` over `ask` over `allow`; else the default is `allow` for `side_effect` `read`
and `ask` for everything else, and `ask` for a name that is not registered. The answer names its
source (`grant`, `policy` or `default`) and the row id.

**Approval.** `tool.request(tool, actor, scope, reason, notify)` returns the decision when it is
already `allow` (fast path, no row); otherwise it inserts a request row with status `requested`
and, when `notify` names an actor and the comm pack is loaded, mails that actor through
`comm.send`. `tool.grant(id, expires_in_s, note)` moves `requested` or `denied` to `granted`,
`tool.deny(id, note)` moves `requested` or `granted` to `denied`, `tool.revoke(id, note)` moves
`granted` to `revoked`; any other transition is refused with the current status in the message.
The decider is the calling actor and must differ from the actor that requested the row: a requester granting its own request is refused with the status and the requester named. `tool.requests(status, actor, tool, limit)` and
`tool.policies(actor, limit)` list. Requests and policies live in two pack-owned tables,
`tool_grants` and `tool_policy`, created by the pack's schema plan; when the general grants
primitive lands, the request rows migrate onto it and this record is amended to say so.

## Acceptance

1. `tool.register` of a new name creates one `project` entity with the four properties and the two
   tags; a second call with the same name returns `created: false` and the same id.
2. Registering with `capabilities` creates missing `capability` concepts once and one `implements`
   edge per pair; `tool.describe` lists them.
3. `tool.suggest` for a need phrased in capability words returns an object registered under a
   different name, with the capability in `via`; the same object registered without the capability
   is not returned by that phrasing (mutation control).
4. `tool.check` on an unregistered name returns `ask` from `default`; on a registered `read` tool,
   `allow` from `default`; with a `deny` policy on `*`, `deny` from `policy` with the policy id; with
   a more specific `allow` policy on the exact name, `allow`.
5. `tool.request` on an `ask` tool inserts one `requested` row; `tool.grant` flips it and
   `tool.check` then answers `allow` from `grant` with that id; after `tool.revoke`, `ask` again.
6. A grant with `expires_in_s` in the past is not active: `tool.check` ignores it.
7. `tool.deny` on a `revoked` row is refused with the current status in the message.
8. `tool.ingest(source="khive")` registers every loaded verb of visibility `verb` under
   `khive:<pack>` with one capability per pack, and a second run reports them all as `existing`.
9. `tool.grant` by the actor that requested the row is refused and the row stays `requested`; the same
   row requested by another actor is granted.

## Known rough edges

Re-registration does not update properties; pattern matching is prefix only; the request mail is
best effort and reported in `notified`; capability expansion reads up to fifty implementers per
concept; the grants table is pack-local until the grants primitive exists. These are listed for the
audit pass, not hidden.
