# ADR-180: Tool Pack: Capability Registry, Ontological Discovery and Use Policy

- **Status**: Accepted (2026-09-09, implemented by the tool pack)
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

## Amendment 1 (2026-09-10): a grant resolves on the registry row, not on the name

The Approval section keys a grant on `(actor, tool)` where `tool` is the registered name, and the
`tool_grants` table stores that name as a string. A name is a label whose meaning is supplied by the
registry row it resolves to, so a grant written that way approves whatever that name means at check
time rather than what the decider saw. Issue #2547 closed the nearest path to changing a registered
meaning in place, and #2545 put the resolved registry row on the exec receipt. This
amendment closes the remaining gap on the decision side.

1. **A granted row pins the object it approved.** `tool.grant` records, on the row it flips, the id
   of the registry entity the name resolves to at that moment and a digest over that row's policy
   inputs: `source`, `side_effect`, `trust` and `schema`, canonically serialized. The digest is not
   over the whole entity: name, description, tags, capabilities and `registered_at` are outside it,
   so linking a capability or editing a description does not invalidate a grant. `tool.request` does
   not pin; the request records what was asked for, and only the decision binds.

2. **`tool.check` resolves an active grant by id.** A `granted` row is active when it has not
   expired, its pinned id equals the id the name resolves to now, and its pinned digest equals the
   digest of that row now. A row failing either comparison is not active and the resolution
   continues to policy and then default, exactly as if no grant existed. The answer's `source` stays
   `grant` only when a grant decided it.

3. **A grant on a name with no registry row is unpinned and ends at first registration.**
   `tool.check` answers `ask` for an unregistered name, so a request and a grant can both be written
   before the name exists. Such a row records a null id and a null digest and is active only while
   the name remains unregistered. The first `tool.register` of that name makes it inactive; it is not
   silently promoted onto the new row, because nobody approved that row.

4. **Why the pin rather than trusting the refusal.** The registry rows are opaque to the generic
   entity verbs today, which makes the pinned digest stable in ordinary operation. That is a property
   of one refusal, not of the grant. The pin states the decision's own assumption, so a later
   relaxation, a restore from a backup, or a second writer inside the owning pack shows up as a grant
   that stops being active rather than as a grant that quietly approves something else.

### Acceptance

10. `tool.grant` on a request for a registered name stores the registry id and a non-null digest;
    `tool.check` then answers `allow` from `grant` with that row id.
11. With the grant active, updating the row's `description` through the owning pack leaves the
    decision `allow` from `grant` (the digest excludes description). Changing `side_effect` makes the
    same check answer from `policy` or `default` instead, and the grant row stays `granted`.
12. A grant written against an unregistered name answers `allow` from `grant`; registering that name
    makes the same check answer `ask`, and a second grant against the now-registered row answers
    `allow` again with the new row id (mutation control: the two grants differ only in the pin).
13. A `granted` row whose pinned id points at a different registered object is inactive: the check
    answers from policy or default, and restoring the correct id on that same row answers `allow`
    from `grant` again.

## Amendment 2 (2026-09-11): the grant digest and the exec receipt canonicalize by the same function

**Status**: Proposed.

Amendment 1 item 1 says the digest covers `source`, `side_effect`, `trust` and `schema`
"canonically serialized", and stops there. That phrase names a property, not a function, and
`schema` is a caller-supplied JSON object whose serialization has more than one defensible form.
The tree already carries four implementations of this property and none is shared. A grep at one ref,
which is a reading and not a census: a recursive key-sorting `canonical` in `khive-mounts`
(`src/catalog.rs`) feeding a `blake3` digest over a four-field tool definition including both
schemas, `pub(crate)` and so unreachable from anywhere else; a recursive canonical writer private to
the moodboard pack, serving checkpoint digests; an exec-side convention stated in prose ("entries
sorted by path, compact JSON") with the bytes assembled by hand at the point of hashing; and a
public `canonical_json` in `khive-vcs` that is specific to archives.

A grant and an exec receipt therefore describe the same registry row through two independent
serializers. If they differ by key order, by whitespace, or by how they render a number that has
more than one representation, the two digests over one unchanged row disagree, and the disagreement
presents as a pin mismatch that cannot be reproduced from the data: the row is intact, the grant is
unexpired, and `tool.check` falls through to policy for a reason nothing in the record explains.

1. **One function, named.** The canonical serialization used by the grant digest is the same
   function the exec receipt uses. It lives in a crate both sides already depend on, is public, and
   states its rules as a contract rather than by example: object keys sorted, no insignificant
   whitespace, and a stated position on non-finite numbers and on duplicate keys, since both are
   reachable from a caller-supplied `schema`.

   The catalog implementation named above is the nearest existing answer and the first thing to
   read: it already digests a four-field definition carrying caller-supplied schemas, order
   insensitively, and its own test pins that property. Promoting it is a real move rather than a
   restatement, because `pub(crate)` is exactly what makes it unreachable from the exec side. A new
   function is written only if promotion cannot serve both callers, and the reason is stated when it
   is. Whether the catalog pin then calls the promoted function is an implementation detail, on the
   one condition that its digest bytes do not move.

2. **Why Amendment 1's acceptance cannot catch this.** Arms 10 through 13 each vary the registry row
   and compare a digest against a digest computed the same way, so they hold whatever the function
   is, including two different functions on the two sides. Every one of them stays green under the
   defect this amendment exists to prevent. That is the reason the constraint needs an arm of its
   own rather than a sentence.

### Acceptance

14. A grant and an exec receipt taken over one registry row agree on the `schema` bytes, compared as
    bytes and not as parsed values. Mutation control, stated before running: pointing either side at
    its own serializer turns this arm red and leaves arms 10 through 13 green, which is the whole
    claim of item 2.
15. Two `schema` objects differing only in key order produce the same digest; two differing in any
    value produce different digests. Both arms named before running, since a serializer that dropped
    the value would satisfy the first alone.
16. A round trip on the shared function: parse, serialize, re-parse, serialize, and require byte
    equality.
