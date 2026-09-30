# ADR-191: Web Pack — Web Ontology, Relation Rules, and Operations

- **Status**: Accepted (2026-09-22, implemented by the web pack)
- **Governing rule**: pack is ontology, relation rules, and operations on those
- **Date**: 2026-09-20
- **Supersedes**: [ADR-175](ADR-175-web-pack.md) and its Amendments 1 and 2 in full
- **Depends on**: [ADR-001](ADR-001-entity-kind-taxonomy.md) (closed entity kinds; pack subtypes),
  [ADR-002](ADR-002-edge-ontology.md) (closed relation set, endpoint contract, certificate),
  [ADR-017](ADR-017-pack-standard.md) (`EDGE_RULES`, `ENTITY_TYPES`), [ADR-028](ADR-028-pack-scoped-backends.md) Amendment 4
  (pack-scoped backends), [ADR-111](ADR-111-blob-store.md) (bodies by content reference)
- **Relates to**: [ADR-085](ADR-085-code-pack.md) (domain-ontology pack shape)

## Context

ADR-175 defined the web pack around one application manifest format: its five entity subtypes were
the manifest's sections, five of its six relation rules connected those sections, its only verb parsed
the manifest, and a storage fence decided which database file the parse landed in. Two amendments
generalised the vocabulary but kept the manifest as the entry point. That is a reader for one file
format, not a web ontology. A pack is an ontology, the relation rules over it, and the operations that
act on those. This record replaces ADR-175 with a pack that describes the web itself and exposes the
web's own actions. Application-level vocabulary (declared tools, capabilities, commerce and identity
protocols, machine-readable manifests) is expressed by consumers on top of this pack through the
extension seam in D6, never inside it.

The governing test applied to every item below: would this exist if no application protocol existed?
An item that fails is out.

## Decision

### D1. Ontology: three entity subtypes

| subtype    | base kind | identity                               | notes                                                                                                                            |
| ---------- | --------- | -------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `site`     | Service   | `(scheme, host, port)`; alias `origin` | the web's unit of identity and policy: credentials, allow-list, ceilings key on it                                               |
| `page`     | Document  | `(site, canonical path+query)`         | a resource whose body is HTML/XHTML                                                                                              |
| `resource` | Document  | `(site, canonical path+query)`         | any other fetched body: `robots.txt`, sitemaps, feeds, JSON, PDF, well-known files, alternate renderings served at their own URL |

Identities are deterministic (UUIDv5 over the identity tuple under the pack namespace) so repeated
fetches and independent ingests converge on the same rows. Canonicalisation: scheme and host
lowercased, default port dropped, path percent-normalised, query kept with keys sorted, fragment
dropped. `?id=1` and `?id=2` are two resources; `#top` is not.

Bodies never live in an entity. Every fetched body is stored through the blob store (content-addressed,
idempotent) and the entity carries `url`, `content_type`, `blob_ref`, `content_digest`, `size`,
`status`, `fetched_at`, and `etag` / `last_modified` when the origin served them. Properties are open:
a consumer may add its own keys to any web entity.

Considered and rejected:

- `origin` as a separate subtype: already an alias of `site`; a second canonical name for one row.
- `representation` (one entity per content-negotiated body): a rendering served at its own URL is a
  `resource`; one served at the same URL under a different `Accept` is a blob plus a receipt (D4).
  An entity per fetch is an unbounded row class with one edge each. The receipt carries `blob_ref`,
  `content_type`, `content_digest` and `fetched_at`, so every negotiated body stays retrievable; a
  consumer that needs a rendering as an entity registers its own Document subtype through D6 and
  links it `derived_from` the page.
- `access_policy`: `robots.txt` is a `resource`; its parsed effect is properties on the `site` and a
  receipt note. An entity cannot be the source of `annotates`.
- `feed` / `enumeration`: a role a resource plays after parsing, not an identity. Parsing emits edges
  (D2), not a subtype.
- `endpoint`: passes the test (forms and REST predate any agent protocol) but collides with the existing
  `api` alias of `service:api`, and its declared form has no web-observable producer in this pack.
  A consumer that learns endpoints from an application declaration registers `service:api` rows through D6.

Registry deletions in `crates/khive-types/src/entity_type.rs` at the superseded revision, by
`(kind, canonical, aliases)`: `(Document, machine_view, [view])`, `(Service, agent_tool, [mcp_tool])`,
`(Document, agent_skill, [skill_manifest])`. Kept: `(Service, site, [origin])`,
`(Document, page, [web_page])`. Added: `(Document, resource, [])`. The registry test that enumerates
web tokens is rewritten as the acceptance witness for exactly this set (A7).

### D2. Relation rules

**One new base relation: `links_to`.** A hyperlink is the web's definitional relation and no existing
relation expresses it without a false claim (`depends_on` would additionally stamp a
`dependency_kind` qualifier the runtime infers for Document→Document pairs). Definition:

| relation   | direction       | endpoint contract   | coherence class                            | cascade |
| ---------- | --------------- | ------------------- | ------------------------------------------ | ------- |
| `links_to` | source → target | Document → Document | state-like (reciprocal links are ordinary) | none    |

One label. A cross-site "mentions" (a reference without an href) is rejected: it is an attribute of a
link, not a second relation, and nothing in D3 produces one. The change touches: the relation enum and
its name list, the `ALL` length assertion (17 → 18), the certificate coverage walk (a disposition entry
for `links_to`), the endpoint-signature tripwire, the ADR-002 relation, category, endpoint-contract and
cascade tables, and every in-tree statement that the relation set has 17 members.

**Pack rules (additive over the base contract): two rows.**

| rule                     | emitted by                                                 |
| ------------------------ | ---------------------------------------------------------- |
| `site contains page`     | fetch, ingest, refresh                                     |
| `site contains resource` | fetch, ingest, refresh, extract (sitemap and feed entries) |

**Base rows the pack uses without declaring anything:**

| relation                                        | web meaning                                                                                               | emitted by             |
| ----------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ---------------------- |
| `page links_to page \| resource` (new base row) | hyperlink                                                                                                 | extract                |
| `document derived_from document`                | extracted text of a page; an alternate rendering at its own URL; a canonical variant with different bytes | extract                |
| `document supersedes document`                  | permanent redirect (301/308): the old address stops being authoritative                                   | fetch, refresh         |
| `note annotates *`                              | fetch/search receipt on the entity it observed                                                            | fetch, search, refresh |
| `note supersedes note`                          | receipt chain: the history of one resource's fetches                                                      | refresh                |
| `service implements concept`                    | a site implementing a named interface; base-covered, no pack rule                                         | consumers              |

Identity is by address: two URLs serving byte-identical bodies are two entities sharing one blob
(deduplication lives in the content-addressed store), and a `derived_from` between them is written only
when a canonical link or a permanent redirect says so. A temporary redirect (302/307) is receipt data,
not an edge.

Rows deliberately NOT carried from ADR-175: `site contains agent_tool`, `site contains agent_skill`,
`agent_skill depends_on agent_tool`, `machine_view derived_from page` — each is an application's
declaration structure; the first three have no web producer and the fourth is the `derived_from` base
row above.

### D3. Operations

All network access is GET or HEAD. The egress rules of ADR-175 Amendment 1 carry over on their own
merits: address classification after resolution (loopback, link-local, private, and metadata ranges
refused), an operator allow-list, bounded redirect chains, decompressed byte and wall-clock ceilings,
credentials bound by configuration to host sets (IP literals match exactly), a response-header
allow-list, and a receipt written after the body's blob is stored.

| verb                                             | contract                                                                                                                                                                                                                 | writes                                                                                                                                                        |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `web.fetch(url, accept?, persist?, max_bytes?)`  | one request with bounded redirects; `persist` defaults true                                                                                                                                                              | `site` if new, `page` or `resource`, blob, receipt note                                                                                                       |
| `web.extract(id \| url, kinds?)`                 | parse a stored body; `kinds` ⊆ {`text`, `links`, `sitemap`, `feed`}, default all applicable                                                                                                                              | text `resource` (`derived_from`), `links_to` edges to targets minted as unfetched `resource` rows (`status` null), `contains` edges from sitemap/feed entries |
| `web.ingest(source, origin?, depth?, limit?)`    | fetch + extract over a URL, a list of URLs, or a served tree on disk (a directory laid out as an origin serves it; `origin` is then required and supplies the `site` identity); `depth` bounds link following, default 0 | as fetch + extract                                                                                                                                            |
| `web.search(query, provider?, limit?, persist?)` | an operator-configured search provider; `persist` defaults false                                                                                                                                                         | hits; a receipt note; with `persist`, each hit's URL as an unfetched `resource` under its site                                                                |
| `web.refresh(id)`                                | conditional re-fetch using `etag` / `last_modified`; unchanged digest writes a receipt only                                                                                                                              | receipt; new blob and updated properties when the body changed                                                                                                |

The `web.refresh` row is amended by Amendment 5.

An unfetched target is minted as `resource` with `status` null. When `fetch` (directly, or through
`ingest` or `refresh`) later retrieves it and the body is HTML, `fetch` updates the row's `entity_type`
to `page` in place; the id does not change because identity is by address. A2's control covers the
re-typing: an extracted target fetched afterwards reads as `page`.

Reads are the graph's own verbs: `search`, `neighbors`, `list`, `get`, `blob.get`. There is no
`web.query`. There is no storage or database parameter on any verb: placement is a configuration
matter (ADR-028 A4 pack-scoped backends), and every write lands in the caller's namespace through the
runtime's create seam, subject to the endpoint contract like any other write. A crawl verb is out of
scope for this record: with `extract` available it is a caller's loop over `fetch`, and its budget and
politeness semantics deserve their own measurement.

Configuration section `[web]`: allow-list, credential bindings by host set, byte and time ceilings,
redirect cap, search provider.

### D4. Receipts

Every network action writes one observation note annotating the entity it touched (or standing alone
for a search): method, final URL, redirect chain, status, content type, negotiated `Accept`, bytes,
timing, egress classification, and the stored reference when `persist` is true, else the content
digest and size (amended by A1.2: a request that stores no body, `persist` false or a HEAD, writes a
receipt with no blob reference and records the content digest, size, final URL and fetch time). Receipts chain by `supersedes`,
so the fetch history of a resource is a note chain (subject to Amendment 9's legacy chain boundary),
and content that did not change produces a receipt
and nothing else (amended by Amendment 5 for changed representation metadata).

### D5. Deletions against the superseded revision

Whole files in `crates/khive-pack-web/src/`: `manifest.rs` (the format's key list), `views.rs`,
`db_target.rs` (storage fence). Rewritten: `extract.rs` (manifest walker, declared-tool and
declared-capability loops, the report seed naming the deleted vocabulary), `persistence.rs` (a raw-SQL
staging path below the verb seam; replaced by runtime create/link calls), `vocab.rs` (rules 2–6),
`handlers.rs`, `pack.rs`. Tests and fixtures under `crates/khive-pack-web/tests/` that carry the
manifest format are removed with it. `docs/packs/web.md` is rewritten. The three registry rows in D1
are removed from `crates/khive-types/src/entity_type.rs`.

### D6. Extension seam

A pack compiled outside this repository against a pinned revision extends the web ontology without
any change here: it implements the pack trait, registers its own entity subtypes (collision-checked
against the registry at boot), declares additional edge rules over the base and web vocabulary (rules
are additive; the pack declares its dependence on `web`), attaches bodies as blobs, and annotates web
entities with its own notes. Its rows carry its own subtype tokens and live in its caller's namespace.
What is public today, verified at the superseded revision: the pack trait and its vocabulary
constants (`khive-types` `Pack`, `EdgeEndpointRule`, `EntityTypeDef`, `REQUIRES`), the runtime half
(`khive-runtime` `PackRuntime`, `PackFactory`, `PackRegistration`), and the boot-time subtype
collision check; a complete pack needs only `khive-types`, `khive-runtime`, `inventory`,
`async-trait` and `serde_json`, and `crates/khive-pack-template` is the working scaffold. Nothing a
pack needs is crate-private.

What is not: pack discovery is a link-time `inventory` registry. A pack is selectable only if it is
linked into the binary and anchored (`kkernel/src/lib.rs` `_pack_links`); a name absent from the
binary is `UnknownPack`. So an out-of-tree pack is used by building a host binary: a thin crate that
depends on the pinned khive crates and the pack, and constructs the server with both. This record adds
the one piece that makes that possible without editing khive sources: `kkernel` exposes its server
construction as a library entry point that accepts additional pack factories (the composition path
`register_packs_with_runtimes` already carries the shape). Dynamic loading (cdylib or WASM) is out of
scope here; it needs an ABI contract and is a separate record. Two pre-existing properties are recorded,
not fixed: pack-declared subtypes reach the composed registry but not `EntityTypeRegistry::global()`,
and channel-ingest grants are hardcoded to the comm pack.

## Acceptance

Controls are stated before the arms run; an arm without its control is not evidence.

- A1 `fetch` of a page mints `site`, `page`, blob, receipt; the same fetch again returns the same blob
  reference and writes a receipt only. Control: a different body yields a different reference.
- A2 `extract(links)` on a page with N distinct hrefs yields N `links_to` edges whose targets are
  resources under their own sites; control: a page with no hrefs yields none.
- A3 a 301 chain yields `new supersedes old`; a 302 yields no edge and a receipt naming the hop.
- A4 `refresh` on an unchanged `etag` writes no entity or blob change; control: changed body updates
  `blob_ref` and `content_digest` (amended by Amendment 5).
- A5 `ingest` of a served tree on disk under a declared `origin` produces the same graph as live
  ingest of the same tree served under that origin over HTTP: the arm asserts id equality row by row
  and edge-set equality.
- A6 the egress arms of ADR-175 Amendment 1, renumbered, unchanged in substance.
- A7 the registry refuses `machine_view`, `agent_tool`, `agent_skill` and their aliases; accepts
  `site`, `page`, `resource`.
- A8 `links_to`: relation count 18; `Document links_to Document` accepted; `Document links_to
  Service` and `Concept links_to Document` refused with the endpoint-contract error, in one test;
  certificate disposition present; endpoint-signature tripwire green.
- A9 with two backends configured, web writes land in the web backend only. Amended by A1.3: attachment rows are the
  exception and land on the main backend.

## Consequences

Query-key sorting means documents such as `...?add=1&mul=2` and `...?mul=2&add=1`, differing only in the order of distinct query keys, resolve to one document; for an endpoint where parameter order is significant this is an accepted loss.

The pack shrinks to what the web is: three subtypes, one new relation, two rules, five operations.
Every application-level concept previously hosted here is expressible on top of it by a consumer pack
through D6, and none of it lives in this repository. The runtime gains one relation, which is the cost of
having a web ontology at all.

## Amendment 1 (2026-09-20): disk ingest confinement, and how stored bodies stay alive

Two normative additions found during implementation review. Both narrow the record; neither changes
the ontology, the relation rules, or a verb signature.

### A1.1 D3: `web.ingest` reads disk only under `[web] read_roots`, decided on the opened descriptor

D3 lets `web.ingest` take a served tree on disk. As written it bounds nothing about which directories
that may be, so the verb would read any file the daemon can read and store it as a `resource` under a
caller-chosen origin. The configuration section `[web]` gains `read_roots`, a list of directory paths.
A disk source is admitted only when it lies under one of them; an absent or empty list refuses every
disk source with a message naming the setting. This mirrors `[exec] read_roots`, the same shape for the
same reason. URL sources are unaffected.

Confinement is a property of the bytes that are read, not of a path that was checked earlier. A check
that canonicalizes a path and then opens it by name again is racy: between the check and the open, a
writer with access to a configured root can replace the checked file with a symbolic link to any file
the daemon can read, and the daemon would store those bytes as a `resource`. So the rule is stated on
the descriptor: the file is opened with symbolic links refused at every path component, the confinement
check is made against the identity of the file as opened (its resolved path read back from the
descriptor, or its device and inode numbers compared with the entry that was checked), and the bytes
stored are read from that same descriptor. A path check followed by a separate open by name satisfies
nothing here. Acceptance A5 keeps its arm and gains two controls: the identical ingest with the tree
outside every root is refused, and a tree in which a regular file is swapped for a symbolic link to a
file outside the root between the listing and the read is refused for that entry and stores no body.
This clause is the contract, not a description of the tree at the time it merges: the web pack change
that implements D3 (pull request #3000) admits disk sources under `read_roots` and reads through the descriptor as stated
here, and cites A1.1 as its acceptance. Until that change lands, D3 disk ingest is unenforced and is
not to be relied on.

### A1.2 D4: bodies are rooted by attachment on the main backend; `persist` false stores no bytes

D4 said every receipt carries a blob reference; this amendment rewrites that sentence of D4 to read
"the stored reference when `persist` is true, else the content digest and size" (the D4 text above
carries the amended wording). D3 says a `page` or `resource` carries `blob_ref`. Neither keeps the
blob alive: blob reclamation consults the attachments table
([ADR-121](ADR-121-attachments-first-class.md)), so a body named only by a property is collectable
once the grace period passes.

The body of a fetched page or resource is that entity's own content in another modality, which is
exactly what ADR-121 makes an attachment. So a persisted `page` or `resource` carries one attachment
with role `content` naming the stored reference, and the receipt note of the request that stored it
annotates the entity (D4) and carries no attachment: the receipt is the utterance, the body is the
thing, and ADR-121's note boundary keeps the two apart. A fetched page is never a note's own content,
so a receipt never carries a body. When the caller asks for no persisted row (`persist` false) no
bytes are stored at all: the body is returned in the response, the receipt records the final URL,
content digest, size and fetch time as properties, and a caller who wants the bytes kept asks for an
entity. A HEAD request stores no body and roots nothing.

Placement follows [ADR-160](ADR-160-shared-pack-infrastructure.md) and ADR-121: the canonical main
backend is the sole owner of attachment rows and the sole liveness authority for the shared blob
store, whatever backend holds the record. A web pack routed to its own backend (ADR-028 Amendment 4)
therefore writes its entity and receipt rows there and its attachment rows on the main backend
through the core accessor, which is also how the pack keeps its bodies alive under one sweep. A9 is
amended below to say exactly that. Because the record and its attachment row live in different
databases, ADR-121's same-transaction delete cascade does not reach across, and
[ADR-073](ADR-073-pack-core-backend-accessor.md) grants no atomicity across backends and asks handlers
for idempotent or compensating writes. So hard-deleting a routed `page` or `resource` is one verb
invocation with two commits in a fixed order: the record's own backend commits the delete first,
then the main backend deletes the attachment rows that named the record, and that second delete is
idempotent (deleting rows that are already gone succeeds). A crash between the two commits leaves
attachment rows whose record is gone; those rows root nothing that matters (the record they would
keep alive no longer exists) but they keep the blob alive until something removes them. Today the
attachment orphan sweep exists as a routine with no production caller, so this leak is unbounded
in time until that sweep is scheduled; scheduling it, with a stated cadence and a count of rows
reclaimed as its artifact, is an obligation this amendment records and does not discharge (tracked
as issue #3038). The
reverse order is forbidden: a crash after the attachment
rows are gone leaves a live record whose body becomes collectable under ADR-121's grace period,
which is data loss, and this amendment exists to make stored bodies stay alive.

**Proposed correction (2026-09-25; ADR-121 Amendment 1).** The preceding scheduling claim does not
bound this leak: the blob orphan sweep counts a still-present attachment row as live even when its
record no longer exists. Removing such rows requires its own reconciliation (#3178). ADR-121
Amendment 1 proposes a scheduled object sweep (#3038) under complete liveness and store-binding
gates; that sweep does not discharge the attachment-row reconciliation. The accepted wording above
remains intact pending the proposed correction.

Acceptance gains three arms: after a fetch with `persist` true the entity carries one `content`
attachment and its receipt carries none; after a fetch with `persist` false no blob is stored, the
receipt carries digest and size, and neither record carries an attachment; hard-deleting a routed
entity leaves no attachment row for it on the main backend.

### A1.3 A9: attachment rows are the one web write that lands on the main backend

A9 reads "with two backends configured, web writes land in the web backend only". Under ADR-160
that is true of entity, note and edge rows and false of attachment rows by design, so A9 is amended
to: with two backends configured, a web pack's entity, note and edge rows land in the web backend
only, and its attachment rows land on the main backend only (ADR-160); the arm asserts both halves.

## Amendment 2 (2026-09-25): which resolved addresses egress refuses, and no ambient proxy

**Status**: Accepted (2026-09-25)

**Context.** D3 carries the egress rules of ADR-175 Amendment 1 over "on their own merits", naming
"address classification after resolution (loopback, link-local, private, and metadata ranges
refused)", and A6 keeps "the egress arms of ADR-175 Amendment 1, renumbered, unchanged in substance".
The rule carried over is ADR-175 A1.2 rule 2: every resolved address is checked, "a loopback,
link-local, private, unique-local, multicast, broadcast or unspecified address refuses; the shared
address space 100.64.0.0/10 counts as private", and "the connection is then made to an address that
passed, never to a fresh resolution of the name". ADR-175 is superseded and is not edited; additions
to the carried-over rule belong here.

Two gaps in the implementation of that rule:

- `classify_address` in `crates/khive-pack-web/src/egress.rs` classifies the embedded IPv4 address
  of an IPv4-mapped IPv6 address (`::ffff:0:0/96`) and of no other IPv6 form. IPv6 addresses that
  reach an IPv4 destination through a translator or a tunnel therefore classify as public whatever
  IPv4 address they carry (#3280).
- `pinned_client` in the same file pins the checked address with `resolve` but does not disable
  proxies, and the HTTP client it builds takes a proxy from the process environment or system
  configuration by default. With a proxy configured, the request goes to the proxy and the checked
  address governs nothing (#3279).

**Decision.**

1. The refused classes are unchanged: loopback, link-local (which includes the metadata address
   169.254.169.254), private, the shared address space 100.64.0.0/10 (as private), unique-local,
   multicast, broadcast and unspecified, checked on every resolved address and on every redirect
   hop.
2. IPv6 addresses that carry an IPv4 destination are classified by that destination where the
   standard fixes its position, and refused as a whole where it does not:
   - IPv4-mapped, `::ffff:0:0/96`: the embedded IPv4 address is classified by the IPv4 rules
     (current behaviour, recorded here).
   - The NAT64 well-known prefix `64:ff9b::/96` (RFC 6052): the embedded IPv4 address, which RFC 6052
     places in the low 32 bits for a /96 prefix, is classified by the IPv4 rules. The IPv6 address
     refuses exactly when that IPv4 address would. RFC 6052 section 3.1 already forbids the
     well-known prefix for non-global IPv4 addresses, so this refuses only addresses the standard
     says should not appear.
   - The local-use translation prefix `64:ff9b:1::/48` (RFC 8215): refused as a whole. The network
     chooses the prefix length inside it, so the position of the embedded IPv4 address cannot be
     read from the address.
   - 6to4, `2002::/16` (RFC 3056): refused as a whole. Its embedded IPv4 address names the relay
     router that decapsulates the packet, not the destination, so classifying it would certify the
     relay and say nothing about where the request lands.

   A refusal names the resolved IPv6 address and, for `64:ff9b::/96`, the embedded IPv4 address and
   its class.
3. The fetch client uses no ambient proxy. The pin in rule 2 is the connection itself, so a client
   never takes a proxy from the process environment or system configuration. Proxied egress is not
   supported; a deployment that needs it needs its own rule stating how the address check still
   binds the destination.

**Alternatives considered.**

- Refuse all of `64:ff9b::/96`. Simplest and fail-closed, but on an IPv6-only host behind DNS64 and
  NAT64 (RFC 6147) every IPv4-only origin resolves only to synthesized `64:ff9b::/96` addresses, and
  one refused address refuses the whole host (`resolve_and_pin` in `egress.rs`). Every IPv4-only site
  would be unreachable on that deployment shape, to avoid a check that is exact for this prefix.
- Decode the IPv4 address embedded in 6to4 as well. It certifies the relay, not the destination
  (Decision 2). 6to4 relaying through the anycast prefix is deprecated (RFC 7526), so the cost of the
  whole-prefix refusal is the few hosts that still publish a 6to4 address, refused as a whole under
  the any-address rule.
- Classify against the full IANA special-purpose address registries, admitting only addresses
  marked globally reachable. Broader: it would also refuse documentation, benchmarking and reserved
  ranges absent from the list above. It changes refusals beyond #3280 and needs its own acceptance
  arms; it is not decided here.
- Honour an operator-configured proxy. Out of scope; see Decision 3.

**Consequences.**

- Newly refused, relative to the implementation: `64:ff9b::/96` addresses whose embedded IPv4
  address is in a refused class, and all of `64:ff9b:1::/48` and `2002::/16`. A process with a proxy
  configured now connects directly to the pinned address. Nothing refused today becomes allowed.
- Not covered by address classification: a network-specific NAT64 prefix (RFC 6052 section 2.3) is
  indistinguishable from ordinary global unicast by address, so on such a network the operator
  allow-list (ADR-175 A1.2 rule 3) is the control. Teredo (`2001::/32`) and the deprecated
  IPv4-compatible form (`::/96`) are not decoded by this rule and classify as ordinary IPv6
  addresses.
- A6 gains arms, controls stated first. Must refuse: `64:ff9b::7f00:1` (127.0.0.1),
  `64:ff9b::a9fe:a9fe` (169.254.169.254), `64:ff9b::a00:1` (10.0.0.1), `64:ff9b::6440:1`
  (100.64.0.1), `64:ff9b:1::1`, `2002:808:808::1`, and a resolver answer carrying one of these beside
  a public address. Must allow: `64:ff9b::808:808` (8.8.8.8) and `2001:4860:4860::8888`. Proxy: with
  a proxy variable set in the process environment the request reaches the pinned address; control, a
  client built without the proxy exclusion sends it to the proxy.

**Refs.** #3280, #3279; ADR-175 Amendment 1, A1.2 rules 2 and 3.

## Amendment 3 (2026-09-25): a HEAD receipt records no digest and no size

**Status**: Accepted (2026-09-25)

**Context.** D4, as amended by A1.2, says "a request that stores no body, `persist` false or a HEAD,
writes a receipt with no blob reference and records the content digest, size, final URL and fetch
time". A HEAD response has no body. ADR-175 A1.1, carried over by D3, says "`HEAD` reads no body and
writes no blob; its reply has `content_ref: null`, `bytes: 0` and `truncated: false`, regardless of a
response's advertised content length". So D4 asks a HEAD receipt for a digest that cannot exist.

The implementation (`crates/khive-pack-web/src/fetch.rs`, `settle`) takes the no-body arm with zero
bytes and no reference, falls back to the absent reference for the digest, and writes
`content_digest: null` and `size: 0` into the receipt. A HEAD that mints a row with no stored body
writes the same pair into the row through `representation_patch`. A size of 0 states that the body
is empty, which the request never observed (#3199).

**Decision.** D4's "content digest and size" applies to a GET with `persist` false. A HEAD writes a
receipt with no blob reference, no content digest and no size (both absent or null), with the final
URL, status, fetch time and the allow-listed response headers, which carry the origin's advertised
`Content-Length` when it sent one. `bytes` in the reply and the receipt stays 0, as ADR-175 A1.1
specifies: it counts bytes read. A HEAD that mints or updates a row with no stored body leaves that
row's `content_digest` and `size` unset. A row that already holds a GET body keeps its body metadata,
as the implementation does today.

**Alternatives considered.**

- Record the advertised `Content-Length` as `size`. Everywhere else `size` counts decompressed bytes
  the pack read (ADR-175 A1.2 rule 5). An advertised length is the origin's claim, and the fetch
  client removes it from responses it decompresses (the HTTP client's documented behaviour for
  transparent decompression), so `size` would be a count for one method, a claim for another, and
  missing for compressed responses. The claim is already in the receipt's response headers.
- Keep D4 as written and change the code: impossible for the digest.
- Keep `size: 0`: it reports an empty body that was never read.

**Consequences.** The receipt and row writes for a HEAD change from `size: 0` to no size. Readers of
`size` see either a count of bytes read or nothing. Acceptance: a HEAD receipt carries neither
`content_digest` nor `size` and its response headers carry the advertised `content-length`; control,
a GET receipt with `persist` false carries both.

**Refs.** #3199, #3160.

## Amendment 4 (2026-09-25): deterministic identity is per namespace

**Status**: Accepted (2026-09-25)

**Context.** D1 says "Identities are deterministic (UUIDv5 over the identity tuple under the pack
namespace) so repeated fetches and independent ingests converge on the same rows", and D3 says
"every write lands in the caller's namespace through the runtime's create seam". Entity ids are
global: by-id resolution carries no namespace check (ADR-007 Rule 2). In
`crates/khive-pack-web/src/identity.rs`, `site_id` is UUIDv5 over the site key alone, `document_id`
keys on the site id and the path and query, and `derived_text_id` keys on the document id; no
namespace enters. Two namespaces that fetch one URL therefore compute one id. `get_or_create` in
`crates/khive-pack-web/src/entities.rs` refuses a row owned by another namespace as not found
(`require_entity_namespace`, whose comment calls it an interim safeguard while namespace-aware
deterministic identity is pending). Today the first namespace to fetch a URL holds it, and every
other namespace's fetch of that URL is refused (#3037).

**Decision.** The identity tuple gains the namespace the rows are written to. `site` identity is
UUIDv5 under the pack namespace over (write namespace, scheme, host, port); `page`, `resource` and
derived-text identities key on the site or document id as they do now, and inherit the namespace
through it. Convergence in D1 and A5 holds within a namespace: repeated fetches and independent
ingests in one namespace converge on the same rows, and two namespaces hold two rows. Bodies still
deduplicate across namespaces in the content-addressed blob store.

**Alternatives considered.**

- Keep URL-only ids and the refusal (the implementation today). A URL can be persisted by one
  namespace for the life of the store; every other namespace loses the pack for that URL.
- Keep URL-only ids and share one row across namespaces. A second namespace's writes would land on
  a row attributed to the first, contrary to D3's "every write lands in the caller's namespace".
- Make the namespace part of a composite storage key instead of the id. Entity ids are global and
  single-column; this changes storage for one pack's convenience.

**Consequences.**

- Rows stored under the URL-only derivation are not re-keyed by this amendment. Once it is
  implemented, fetch, ingest and refresh derive the namespaced id, so a URL fetched again gets a new
  row; the earlier row stays readable by id and by search until removed. Whether stored rows warrant
  a re-keying migration depends on how many exist and is not decided here.
- `require_entity_namespace` no longer triggers for ids derived under this rule and stays as a guard.
- Acceptance: the same URL fetched in two namespaces yields two `page` rows with different ids and
  one blob; control, the same URL fetched twice in one namespace yields one row. A5 runs within one
  namespace.

**Refs.** #3037.

## Amendment 5 (2026-09-27): refresh metadata and redirected 304

**Status**: Accepted (2026-09-27)

**Context.** D3 and A4 say an unchanged body writes only a receipt. A response can keep the body
bytes while changing `Content-Type`, status, `ETag` or `Last-Modified`; ignoring those fields leaves
the stored representation stale and sends an obsolete validator on the next refresh (#3093). D3
also does not retain the fetched body's `Accept` and `Accept-Language`, so refresh can ask for a
different representation from the one whose body it holds. Identity is by address under D2 and D3:
a redirect target is a different row, and a 304 from it supplies no body for that address. The
redirected-304 ruling of 2026-09-27 selects refusal over copying the source body to the target.

**Decision.**

1. A persisted GET stores the representation negotiation sent with the request: lowercase
   `accept` and `accept-language` keys, each an array retaining repeated values in sent order.
   Only a later caller-issued persisted GET replaces this map, including clearing values absent
   from its request. `web.refresh` replays the stored negotiation and never replaces the map.
   HEAD records its own request in its receipt but does not replace the cached GET body's
   negotiation. Credentials, conditional validators and all other request headers are excluded
   from this stored map.
2. `web.refresh` replays the stored `Accept` and `Accept-Language` on each redirect hop. It sends
   the source row's `If-None-Match` and `If-Modified-Since` only on the first request to the stored
   address, and only when the cached body is known complete. A partial body is fetched without
   validators. A 304 after any redirect is refused as `redirected_not_modified` before source or
   target graph rows, attachments, or receipts change; the caller fetches the target instead.
3. A body response with the same content reference and completeness can still patch changed
   representation metadata. It applies supplied `Content-Type` and the representation status;
   a 200 response replaces `ETag` and `Last-Modified`, clearing either validator when absent.
   A 304 patches only header fields it supplies, retains the cached representation status, and
   records its actual 304 status in the receipt. A 304 with no metadata change writes only the
   receipt. Metadata-only patches retain the blob reference and content attachment; the reply's
   `changed` flag continues to describe a body/reference change.
4. Refresh settles any response body before patching metadata. The metadata patch is conditional
   on the terminal row still carrying that response's content reference and on the entity snapshot
   the refresh started from, or its own body-settlement write, remaining unchanged. A mismatch
   skips the metadata write and sets `lost_race: true` in the reply and receipt. The revision
   fence covers representation metadata and validators even when a later GET stored the same body
   bytes; a fresh read after the response is not the refresh's starting snapshot.
5. Fetch and refresh receipts carry the allow-listed response headers and the request's
   allow-listed negotiation. A body-storing receipt also carries `body_entity_id`, the terminal
   document that owns its D4 `content_ref`; a bodyless receipt has no body owner. They do not
   record credential headers.

**Alternatives considered.**

- Keep D3's receipt-only rule whenever body bytes match. This strands a new validator and
  content type until a future changed-body response and repeats the stale conditional request.
- Replay the source validators on redirect hops and accept a target 304. A target has its own
  address identity; its 304 cannot establish that it served the source row's cached bytes. A
  cached target's own validators could support a separate target-specific validation rule, but
  this amendment does not define or implement that rule.
- Copy the source body to an uncached redirect target after its 304. That would mint a fetched
  target representation without receiving a body from the target.
- Retain negotiation only in the receipt chain. Refresh would have to reconstruct the cached
  body's request context from mutable history rather than read it beside the body reference.

**Consequences and acceptance.** A same-body 200 changing type, status or validators updates
only representation metadata; the attachment and blob reference remain unchanged. A 304 with a
new ETag updates that field but preserves cached status; a bodyless 304 with no new metadata writes
only a receipt. The next refresh sends the updated validator and the cached GET negotiation.
HEAD leaves that negotiation intact, while a later unnegotiated persisted GET clears it. For every
301, 302, 307 and 308 hop, a terminal 200 updates the target's representation and leaves the
source's representation fields alone; a terminal 304 refuses with both rows and the receipt chain
unchanged. A control that resends source validators after a redirect must fail the first-hop
header test; a control that removes the 304 refusal must fail the no-mutation test; a control that
skips same-body metadata must fail the metadata test; a control that drops the body/metadata
revision guard must fail the overlapping-fetch test.

**Refs.** #3093, PR #3165, ADR-191 D2-D4 and A4, the 2026-09-27 redirected-304 ruling.

## Amendment 6 (2026-09-27): capture-bound extraction evidence

**Status**: Accepted

### Context

ADR-191 D3 keys the extracted-text `resource` on its source document id. A later body at the same URL overwrites that row, even when the two HTML bodies yield identical text. `web.extract` records no extraction receipt, so its derived row cannot name the stored body or fetch receipt that supplied it. The `links_to` relation is presently a live, unqualified set: extraction adds targets, but never retracts a target absent from a newer body. The HTML parser sees only `<a href>`; it drops `rel`, anchor context, repeated occurrences, `<link>` and HTTP `Link` fields. The response-header projection drops `Link` as well.

### Decision

1. Each completed or degraded `web.extract` writes an immutable `observation` note tagged `web.extraction`. It annotates the source document and any derived-text row, and records the exact source `ContentRef`, the receipt id of the capture that stored that body when one is provable, selected kinds, per-kind results, and the extracted link evidence or structured refusal. The extraction note has a `source` attachment to the verified input blob, so a later fetch cannot make the historical body eligible for blob collection. Before hydration and again after parsing but before derived writes, extraction requires the document's `blob_ref` property and `content` attachment projection to agree with that input reference; a mismatch refuses with `capture_changed`. A legacy document with no provable capture receipt records a null receipt id, but still records and roots the exact body reference. Extraction receipts do not enter the `web.receipt` network receipt chain.
2. The extracted-text `resource` identity is UUIDv5 over `(source document id, source ContentRef)`. It records both and the capture receipt id if known when the row is created. Re-extracting one body converges; a different body produces a different row even if its text bytes match. Its `derived_from` edge still points to the address-identity document. Its own `content` attachment roots the excerpt. Subsequent extraction notes identify subsequent captures of the same bytes without changing the earlier note.
3. `links_to` remains one live triple per source and target. Extraction owns triples marked `web_extract=true`. A live unmarked triple is claimed only when its target is present in the **full parsed distinct-target set** and its stored shape is byte-identical to the legacy extractor write: `metadata=None` and `weight=1.0`. Presence is required even when the target is skipped by the admission budget. An unmarked edge with caller metadata or a nondefault weight is not claimed or overwritten; an absent unmarked edge is not claimed; an unmarked tombstone is not resurrected. The extraction note records every claim under `legacy_claimed` with edge id, target id, and admission state, and every observed unclaimed live edge or admitted-target tombstone under `legacy_unclaimed` with live, presence, and collision evidence. Unclaimed edges require explicit curation before extraction may own them.

   Each admitted target without an unclaimed collision has one extractor-owned edge whose metadata carries the selected body reference, capture receipt id, occurrence count, and ordered occurrence evidence. Each occurrence carries source (`anchor`, HTML `link`, or response `header`), raw href, normalized `rel` tokens, bounded normalized anchor/title context, and whether context was truncated. Repeated hrefs to one target increase the occurrence count rather than adding another edge. The extraction note retains that run's link evidence after live metadata changes or an edge is soft-deleted. Only marked edges absent from the full parsed target set are soft-deleted; reappearance explicitly resurrects marked tombstones. A budget-skipped target still present in the body or response headers keeps its live edge, is recorded with `admitted=false`, and, if default-shaped and unmarked, is claimed with provenance but without admitted occurrence metadata. `edges_created` counts admitted extractor-owned edge writes, `admitted_targets` counts budget use, and `ownership_collisions` counts admitted targets blocked by unclaimed triples. `links_complete=false` for an admission collision or budget skip. The live-set acceptance applies to extractor-owned edges; an unclaimed edge may remain live when absent from the latest capture.
4. The allow-listed fetch and refresh receipt header projection includes every response `Link` field. Link extraction reads it from a capture receipt, along with `<a>` and `<link>` elements in the stored HTML. A body-storing network or disk receipt records `body_entity_id` as the terminal document that owns its `content_ref`; redirect participants may still be annotated by that receipt. Amendment 5's D4 receipt allow-list includes this body-owner field for body-storing receipts and no owner for bodyless receipts. Extraction accepts a capture receipt only when its body reference and `body_entity_id` match the source document and a live annotation connects that receipt to the document. The document's `capture_receipt_id` is a guarded convenience pointer: it is updated only while both `blob_ref` and the `content` attachment still match the receipt's body, and a concurrent mismatch leaves the receipt in history without rebinding the pointer. A legacy receipt lacking provable body ownership is not guessed from digest equality or annotation alone. A matching newer body receipt is preferred. Amendment 5's same-body representation-metadata semantics remain authoritative; this amendment adds extraction provenance and `Link` evidence without changing that rule. Amendment 9 tightens receipt trust: the web receipt marker is required in addition to these body-owner checks, and generic note writes cannot establish it.
5. `link_limit` remains a shared distinct-target admission budget in requested kind order, capped at 1,000. The extraction receipt marks `links_complete=false` if syntactically valid distinct targets were skipped by that budget, while retaining their evidence with `admitted=false`. A separate 10,000-occurrence ceiling refuses only the `links` kind before link writes or ownership reconciliation. The call returns a `degraded` result with a structured `too_many_link_occurrences` refusal; other selected kinds may complete. Its extraction note still records the refusal and roots the input body. Because the full target set was not parsed, `legacy_claimed` and `legacy_unclaimed` are null rather than incomplete lists. Anchor/title context is at most 256 UTF-8 bytes and carries a truncation flag. The source body remains available for consumers needing the unabridged context.

### Alternatives rejected

- **Document-id text identity plus `supersedes`:** a single mutable derived row still loses which source body produced earlier equal text. Chaining mutable rows would create a new identity scheme implicitly and require ordering a derivation history that is naturally keyed by the immutable body reference. Source-ContentRef identity gives convergence for repeated extraction of the same bytes and separates different captures.
- **A new `links_to` edge per capture:** the base graph's `links_to` triple has one natural key `(namespace, source, target, relation)` and D2 defines this relation as state-like. Per-capture parallel edges would change that storage contract and make ordinary `neighbors` and `web.ingest` follow historical links. Keeping one live edge and immutable extraction notes separates current traversal from capture history.
- **Only copying the current link set onto the document:** a mutable property cannot retain earlier relation values or header evidence after refresh. The extraction note is the durable historical record; edge metadata serves current graph queries.
- **Treating every unmarked `links_to` edge as extractor-owned, or claiming one on presence alone:** an unmarked triple can have been written by a caller. A natural-key upsert would replace its metadata and weight before ownership could be recovered, while a later extraction would delete it. The narrow `(metadata=None, weight=1.0)` predicate admits only the legacy extractor's exact stored shape, and only on parsed presence. Other live shapes and all unmarked tombstones retain their ownership ambiguity for curation.

### Consequences

The body concordance checks fail closed when a capture changes during hydration or parsing. They are not a transaction spanning text creation, link upsert, link retraction, note creation, and the note's source attachment. Concurrent captures can interleave after the pre-write check, and a caller can create an unmarked edge after ownership inspection but before the upsert (#3455). The immutable extraction note and per-edge source reference make such history diagnosable; they do not make live-edge replacement fully serializable. A future guarded graph mutation that compares the document body and edge ownership in one writer transaction is required for that guarantee.

### Acceptance and controls

- Fetch body A, extract text and links, fetch body B whose extracted text equals A's but whose bytes and links differ, then extract again. The two derived-text ids and extraction notes differ; each note names its exact input `ContentRef` and capture receipt, and its `source` attachment still retrieves that body. The first link set remains in its note; only B's targets are live. **Control:** remove the ContentRef from derived identity or skip missing-link retraction; the assertions fail.
- Extract one page with two links to the same target carrying different `rel` and anchor text, an HTML `<link rel=stylesheet>`, and a response `Link` field. The graph has one live edge per target with all relation values and occurrence counts, and the extraction note retains the same evidence. **Control:** omit `rel`, suppress HTML `<link>`, or drop the response `Link` field; the assertions fail.
- A repeated extraction of an unchanged capture reuses the text id. A HEAD or 304 after a body capture does not relabel that body as the HEAD/304's body. A removed marked link that reappears becomes live again. Lowering `link_limit` keeps previously live targets still present in the full parsed set, while recording their current evidence with `admitted=false`. **Control:** select only the latest receipt without matching its body reference, leave resurrection disabled, or compare only with admitted targets; the assertions fail.
- Begin with three unmarked live edges: present with `metadata=None, weight=1.0`; present with caller metadata; and absent with `metadata=None, weight=1.0`. Extract with enough budget to admit both present targets. Only the first is claimed and recorded under `legacy_claimed`; the caller edge retains its metadata and appears as an ownership collision; the absent default edge stays untouched under `legacy_unclaimed`. A later body with neither target retracts only the claimed edge. **Control:** drop the exact legacy-shape predicate, claim an absent default edge, or retract an unclaimed edge; the assertions fail.
- A present unmarked edge with `metadata=None` but nondefault weight remains unclaimed; an unmarked tombstone stays deleted. A present default-shaped live edge beyond `link_limit` is still claimed with `admitted=false` and is retracted when absent from the next parsed capture. **Control:** ignore weight, resurrect the tombstone, or restrict claiming to admitted targets; the assertions fail.
- Give two documents an identical body digest and a receipt that annotates both but names only one as `body_entity_id`. Extraction of the other document must not select that receipt or parse its `Link` header; even a stale `capture_receipt_id` pointer to it resolves to no capture. **Control:** match by digest or annotation without body owner; the assertions fail.
- Change only the document's `blob_ref` while its content attachment still names the old body; extraction refuses before derived writes. Replace both during a paused hydration read; the post-parse check refuses before derived writes. **Control:** omit attachment concordance or the second pre-write read; the assertions fail.
- Supply 10,001 link occurrences. No link edge or ownership mutation occurs; the response and immutable extraction note report `degraded` with `too_many_link_occurrences`, and the note's `source` attachment roots the refused input. **Control:** return early before writing the note or continue link writes after the ceiling; the assertions fail.

## Amendment 7 (2026-09-27): response selection context for cached web bodies

**Status**: Accepted (2026-09-27)

**Context.** Amendment 5 binds a persisted GET body's `Accept` and `Accept-Language` request values to its cached representation and replays those values on refresh. That map alone cannot establish that a conditional response validates the same selected representation: a server may name additional selecting fields in `Vary`, use `Vary: *`, or change its selector in a 304. The fetch and refresh response allowlist also omits `Vary` and `Content-Language`, so receipts cannot show that selection context (#3448). A legacy body may have no trustworthy request map or recorded `Vary`. The HTTP client fixes `Accept-Encoding: gzip` on each request; the egress header allowlist does not permit a caller to change it.

**Decision.**

1. Fetch and refresh receipts expose `Vary` and `Content-Language` in their allowlisted response-header projection. The `Vary` value retains every field line in order as a JSON array; a line that is not valid UTF-8 remains an explicit `null` entry. Repeated valid `Content-Language` field lines are combined in order with `,`; an invalid supplied line is represented as `null`. No other response header is added to this projection.
2. A persisted GET stores the response's `vary` and `content_language` beside that body's `blob_ref` and Amendment 5 `request_headers` map. An observed response without `Vary` stores `vary: []`, distinct from a legacy or interrupted body with no trustworthy `vary`. A missing `Content-Language` stores `content_language: null`. A HEAD response records its own headers in its receipt but does not change the cached GET body's selection context. Disk-ingested content records the known-empty HTTP selection context. A body revision is initially marked with an unreplayable `vary` until its response context is bound, so a failed or competing context write cannot leave the previous body's selector available for conditional use.
3. A 200 refresh replaces `vary` and `content_language` with its response values, clearing fields omitted by that response. A 304 changes either field only when the response supplies it; absent fields remain the cached body's values. web.refresh never replaces the SOURCE row's negotiation map. When a refresh receives a redirect terminal's body with a 200, the terminal row's map becomes the negotiation sent to it, as a persisted GET would set it; a terminal 304 is refused before any write (A5 D2). An invalid or missing legacy source map causes an unconditional GET on every refresh and is preserved across those refreshes. Only a later caller-issued persisted GET can replace that source map.
4. A refresh sends cached `ETag` or `Last-Modified` validators only for a complete body at the original address's first hop and only when the stored request map and `vary` can be replayed. Every member of every `Vary` field line must be a syntactically valid field name in the supported set `Accept`, `Accept-Language`, and `Accept-Encoding`, and the corresponding stored request field must have valid, nonempty value(s). `Accept-Encoding` is fixed to `gzip` by the client on every request and is therefore always represented in a new HTTP GET's request map; a stored encoding other than that fixed value is not replayable. A future change allowing a caller to choose an encoding must record the actual sent value in the request map before treating that `Vary` member as represented. The empty `vary: []` is replayable when the stored request map is valid. `Vary: *`, any other field, unknown or malformed members, invalid UTF-8, a missing selector record, a malformed request map, and a missing or invalid varied request value all suppress validators and cause an unconditional GET. Negotiation replay follows Amendment 5; an invalid legacy map supplies no replayable negotiation.
5. A 304 without an actually sent validator is refused as `unsolicited_not_modified`. A 304 whose supplied `Vary` selects any field that the actual request did not represent is refused as `unrepresented_vary`. The fixed `Accept-Encoding: gzip` counts as represented on the wire even when a valid legacy request map lacks that field; the legacy map is not repaired, so a newly recorded `Vary: Accept-Encoding` makes the next refresh unconditional. Both refusals occur before graph, attachment, metadata, or receipt mutation. Amendment 5's redirected-304 refusal remains in force.

**Alternatives considered.**

- Keep only the first `Vary` line or drop an undecodable line. Either can falsely classify a response as replayable after losing a selector.
- Treat absent legacy `Vary` as an observed empty selector. Legacy rows did not record the field, so absence is not evidence that the origin omitted it.
- Infer a safe selector from a 304 after sending no validator. A bodyless response cannot establish which cached representation it validates without a conditional request.
- Repair an invalid stored request map during refresh. That would silently rebind the cached GET body to request negotiation it did not record, contrary to Amendment 5. Unconditional refresh preserves the map until a caller-issued persisted GET replaces it.
- Treat `Accept-Encoding` as unsupported even while the client always sends fixed `gzip`. That would force an unconditional GET for a selector the client can represent and replay.

**Consequences and acceptance.** A GET with repeated `Vary` lines and `Content-Language` persists those values and projects them into its receipt; a later HEAD leaves the cached context unchanged. A same-body or changed-body 200 refresh clears omitted selection metadata, while a 304 updates supplied fields and retains omitted fields. Conditional headers are present for a complete body whose stored `Vary` is empty or wholly represented by valid stored `Accept`, `Accept-Language`, and fixed `Accept-Encoding: gzip`, and absent for `*`, any other field, malformed selectors, missing maps, and missing or invalid varied values. An invalid legacy request map remains unchanged after repeated unconditional 200 refreshes. A 304 without a sent validator or with a new unrepresented selector leaves the entity and receipt count unchanged. The source controls are in `crates/khive-pack-web/src/refresh_metadata_tests.rs`; the selection and projection functions are in `crates/khive-pack-web/src/fetch.rs` and `crates/khive-pack-web/src/refresh.rs`.

**Refs.** #3448; ADR-191 Amendment 5; [RFC 9110 §12.5.5](https://www.rfc-editor.org/rfc/rfc9110.html#section-12.5.5); [RFC 9111 §4.1](https://www.rfc-editor.org/rfc/rfc9111.html#section-4.1) and [§4.3.4](https://www.rfc-editor.org/rfc/rfc9111.html#section-4.3.4).

## Amendment 8 (2026-09-29): web requests ask for identity encoding and refuse any declared content coding

**Status: Proposed (2026-09-29).**

**Context.** The web pack's HTTP client used to send `Accept-Encoding: gzip` and decode the response before the byte bound was applied. ADR-175 A1.2 rule 5 gave the reason: a small compressed response can expand without limit, so a bound on the encoded stream bounds nothing the caller sees. The decoding was done by the `compression-codecs` crate, which `reqwest` reaches through its `gzip` feature. In `compression-codecs` releases through at least 0.4.43 (the parser reads the same in 0.4.38, 0.4.41 and 0.4.42; this workspace locked 0.4.42) the gzip header parser takes the parsed header state before it reads the header-CRC (FHCRC) flag. The flag therefore always reads as unset, and the two header-CRC bytes go to the inflater as compressed data. A response whose gzip header sets that flag can then decode into bytes that are not the origin's content, or fail to decode, depending on other header fields. In the first case the client reports no error, so the pack would store those bytes under a content digest and a receipt as if the origin had sent them. The requirement behind rule 5 stands: the byte bound must limit what the caller receives. This amendment meets it without a decoder. Nothing is decoded, so the bytes read are the bytes the origin sent.

**Supersedes.** Line numbers are those of the two files when this amendment was written. ADR-175 and the earlier text of this record are not edited; the sentences below are replaced as described.

- ADR-191 D3, line 119: "decompressed byte and wall-clock ceilings". The byte ceiling counts bytes as received (Decision 4).
- ADR-191 Amendment 3, lines 429 to 433: "Everywhere else `size` counts decompressed bytes the pack read (ADR-175 A1.2 rule 5)" and "the fetch client removes it from responses it decompresses". `size` counts bytes as received, the client decompresses nothing, and a response with a content coding is refused. Amendment 3's decision is unchanged: a HEAD receipt records no digest and no size, because an advertised `Content-Length` is the origin's claim and `size` records bytes the pack read.
- ADR-191 Amendment 7, lines 604, 611, 612, 620 and 622: "The HTTP client fixes `Accept-Encoding: gzip` on each request", "`Accept-Encoding` is fixed to `gzip` by the client on every request", "The fixed `Accept-Encoding: gzip` counts as represented on the wire", and the two later mentions of the fixed `gzip` value. The fixed value is `identity`. Decision 5 states what happens to a stored `gzip` value; its narrow source-map exception follows below.
- ADR-191 Amendment 5 Decision 1 and Amendment 7 Decision 3 say refresh never replaces the source body's negotiation map. Decision 5 makes one exception: after a valid legacy `gzip` map's unconditional identity GET returns a 200 body, refresh binds the negotiation actually sent to that replacement body. Other source maps retain the earlier rule.
- ADR-175 A1.2 rule 5, lines 315 to 318: "The time bound covers the whole read, including redirects and decompression. The byte bound is on decompressed bytes ... decoding stops at the bound". The time bound covers the whole read, including redirects. The byte bound is on bytes as received.
- ADR-175 A1.3, line 353: "decompressed response-byte bounds". Search has the same byte bound as fetch, on bytes as received.
- ADR-175 acceptance arm 19, lines 435 and 436: "A compressed response whose decompressed size exceeds the byte bound is stored truncated". The refusal in Decision 3 replaces this arm.
- ADR-175 acceptance arm 26, line 455: "an over-byte decompressed response". The arm applies to an over-byte response as received.

**Decision.**

1. Every request the web pack sends carries `Accept-Encoding: identity`. This covers `web.fetch`, `web.refresh`, `web.search` and the URL sources of `web.ingest`, which fetch through `web.fetch`. It applies on every hop, including redirects and HEAD. A caller cannot supply another value.
2. The HTTP client decodes nothing. The `gzip` feature of `reqwest` is removed from `khive-pack-web`, so no decoder is compiled in, and the `async-compression`, `compression-codecs` and `compression-core` packages no longer appear in `Cargo.lock`.
3. A GET response that declares any content coding other than `identity` is refused with the error `unsupported_content_encoding`, before any body byte is read. Every `Content-Encoding` field line must list only `identity`, matched without regard to letter case and ignoring whitespace at either end of a member and empty list members. A field line that cannot be read as text is refused. The refusal names the declared value. The check does not apply where no body is read or no content exists: a redirect that carries a `Location` the client can follow, a HEAD response, and a 204 or 304 response. A body that declares no coding is never decoded, whatever its first bytes look like.
4. The byte bound counts bytes as received. `max_bytes` and the operator ceiling limit the bytes read from the connection, with no decoding step between the connection and the bound. A response longer than the bound is still stored truncated with `truncated: true`, as ADR-175 A1.2 rule 5 says. `size`, `bytes` and the content digest describe the bytes stored.
5. A new persisted GET records `accept-encoding: ["identity"]` in its stored request map (Amendment 5 D1, Amendment 7 D2). A stored map that records the earlier value `["gzip"]` stays valid for negotiation replay, but its cached body may have passed through the old decoder. Its next refresh sends `identity` **without any conditional validator**, even when `Vary: Accept-Encoding` is represented. A bodyless 304 in response to that unconditional request is refused and cannot mark the old body current. When a valid legacy map's unconditional identity GET returns a 200 body, refresh binds the negotiation actually sent, including `["identity"]`, to that body even if its digest matches the old body. A complete replacement body may then be validated on later refreshes under the normal `Vary` and completeness gates. A failed or bodyless request does not rewrite the old map; malformed legacy maps remain unreplayable and are not repaired by refresh. Any other stored `accept-encoding` value, or more than one value, is not replayable, as before.

**Alternatives considered.**

- Keep requesting gzip until the decoder is corrected. A decoded body is stored under a digest and read later as the origin's content, so a decoder defect changes stored data with no error. Identity costs only the bandwidth that compression saved.
- Accept a coded response and store its bytes undecoded. The stored object would not be the representation that the receipt's content type describes, and every reader would have to know to decode it. A refusal states the condition at the point it occurs.
- Refuse only `gzip`. The client decodes no coding, so a body labelled with any other coding would reach the byte bound as undecoded bytes in the same way.
- Send the legacy body's validator while changing its request coding to identity. A 304 carries no replacement body and cannot establish that bytes decoded under the old client are the identity representation.

**Consequences and acceptance.** An origin that answers a request for identity with a coded body is refused with `unsupported_content_encoding`, where before the pack decoded that body. Compressible responses cost more bytes on the wire. Bodies stored by earlier fetches are unchanged until an unconditional identity GET replaces them; a legacy `gzip` request map remains readable but cannot authorize a 304. The controls are in `crates/khive-pack-web/src/`:

- `fhcrc_probe_tests.rs`:
  - `gzip_streams_with_a_header_crc_are_refused_across_mtimes_and_caps`: gzip streams that set the header-CRC flag, with a correct and with a wrong header CRC, across every low mtime byte value, values that set each higher byte, and four byte caps (1, 1300, the plaintext length, and one more than it), are each refused as `unsupported_content_encoding`, and the request offered `Accept-Encoding: identity` alone. Two controls inside it must fail on their own inputs: a decode error is not accepted as the named refusal, and a request offering `gzip` fails the identity assertion.
  - `plain_gzip_without_a_header_crc_is_refused`: the same scan for streams that do not set the flag.
  - `every_declared_content_coding_is_refused`: `gzip`, `GZIP`, `x-gzip`, `br`, `deflate`, `zstd`, `compress`, `aes128gcm`, and lists such as `identity, gzip` and `identity,identity,br`.
  - `identity_responses_return_the_exact_plaintext_prefix_at_each_cap`: a response with no `Content-Encoding`, with `identity` in either letter case, or with an empty value returns the received bytes cut at the cap, with `truncated` set exactly when the body is longer than the cap. Gzip bytes served as identity come back undecoded.
  - `head_response_declaring_gzip_is_not_refused`: a HEAD response is accepted without a body-decoder refusal.
  - `not_modified_response_declaring_gzip_is_not_refused`: a 304 response is accepted at the fetch hop because no body is read; refresh still applies its own validator gate.
  - `no_content_response_declaring_gzip_is_not_refused`: a 204 response is accepted without a body-decoder refusal.
  - `redirect_declaring_gzip_is_returned_and_followed_to_the_identity_response`: a followable redirect is returned and followed; its terminal identity response is read without decoding.
  - `redirect_without_a_location_declaring_gzip_is_refused`: a redirect that cannot be followed is refused before its declared coded body is read.
- `fetch.rs`:
  - `arm19_gzip_response_is_refused_not_passed_through`, formerly `arm19_gzip_response_truncates_after_decompression_to_the_bound`: a gzip response is refused by name and no compressed byte reaches the caller.
  - `plain_and_pinned_clients_send_fixed_accept_encoding`: the built clients alone offer no coding, and `run_one_hop` sends `identity` exactly once on each GET and HEAD hop.
- `refresh_metadata_tests.rs`:
  - `stored_gzip_encoding_map_replays_identity_without_validators`: a stored map recording `["gzip"]` sends `identity` with no validator.
  - `legacy_gzip_body_requires_identity_get_before_validation`: an unsolicited 304 cannot bless the old body, a subsequent GET body records the identity map, and only then may a later refresh send its validator. A control that restores validator sending for the legacy map must fail this arm.
  - `legacy_map_304_represents_wire_identity_but_next_refresh_is_unconditional`, formerly `legacy_map_304_represents_wire_gzip_but_next_refresh_is_unconditional`.
  - `vary_gate_requires_every_stored_selector_and_value`: a stored `["br"]` value is not replayable.
  - `vary_accept_encoding_with_fixed_record_sends_validator`: a stored `["identity"]` value sends its validator.
  - The assertions on recorded `request_headers` in this file, and in `a5_literal_http_served_tree_parity_id_and_edge_set_equality_with_disk_ingest` in `ingest.rs`, expect `identity`.

**Refs.** #3587; ADR-175 Amendment 1 (A1.2 rule 5, A1.3, acceptance arms 19 and 26); ADR-191 D3 and Amendments 3, 5 and 7; the gzip header parser in `compression-codecs` through at least 0.4.43 (`src/gzip/header.rs`).

## Amendment 9 (2026-09-29): web receipt provenance and the legacy chain boundary

**Status**: Proposed; pending Leo sign-off.

**Context.** A note with kind `observation`, tag `web.receipt`, matching request fields and an `annotates` edge could previously be written through generic `create`. Its shape alone did not prove that a web operation observed the claimed response. Refresh could reuse its validators and extraction could treat its response `Link` fields as capture evidence (#3499). Receipts already stored before this provenance change have the same unmarked shape.

**Decision.**

1. A web-written network or disk receipt carries the reserved top-level note property `khive:web_receipt: "v1"`. The dedicated runtime `create_web_receipt_note` operation, used by the web pack's receipt writer, establishes the marker with the fixed observation kind and `web.receipt` tag before the note's first storage write. The marker is provenance of that writer path, not a claim inferred from the tag, request shape, body digest, body owner or annotation. A generic note `create` cannot supply the reserved key. Generic note update and merge cannot add, remove or alter the marker or any field of a marked receipt; the public note-store properties-write seams also refuse marked receipt mutation. Web receipts are immutable through those generic paths. This marker is not a cryptographic attestation of the remote response or an in-process capability token: Rust code with direct access to the dedicated runtime operation can call it.
2. Refresh selects only a marked receipt as the prior network receipt. Extraction accepts a receipt as capture evidence only when it has that marker **and** meets Amendment 6's body-reference, body-owner and live-annotation checks. A `capture_receipt_id` pointer alone never makes an unmarked note eligible. An unmarked note may remain readable as a historical note, but it supplies neither validators nor response `Link` evidence to these consumers.
3. No migration infers or backfills the marker on pre-change receipts. Existing unmarked receipts remain stored with their edges, but selection treats them as absent. If a document has no marked receipt, its first successful post-upgrade refresh writes a new marked receipt with no `supersedes` edge to the old unmarked receipt. Later refreshes chain to the latest marked receipt, beginning a new D4 chain; the old history is not deleted or joined by a guessed edge. For an existing body with no proven marked capture, `web.extract` still extracts from the concordant document body and writes its extraction note, but records `capture_receipt_id: null` and does not read `Link` headers from the unmarked receipt. HTML body links remain eligible. A later body-storing web fetch or refresh can establish a marked capture for subsequent extraction.

**Consequences and acceptance.** The provenance boundary deliberately splits pre-change and post-change receipt chains and can temporarily remove recorded HTTP `Link` evidence from extraction until another body-storing capture occurs. Legacy documents with no marked receipt take the filtered lookup's no-match path; the current store query can scan older note rows on that path, so this amendment does not promise a constant-cost legacy lookup. Preserve a legacy unmarked receipt and its annotation, refresh the document twice, and verify that the first marked receipt does not supersede the legacy note while the second supersedes the first. Extract a concordant legacy body with a response `Link` field only on its unmarked receipt: extraction succeeds with a null capture id and creates no link from that header. **Controls:** selecting by tag alone would join the old chain or admit the header; trusting `capture_receipt_id` alone would admit an unmarked receipt.

**Refs.** #3499; ADR-191 D4 and Amendment 6 item 4; PR #3625.
