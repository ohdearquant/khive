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
| `web.refresh(id)`                                | conditional re-fetch using stored negotiation and `etag` / `last_modified`; unchanged digest retains the body and updates changed representation metadata                                                                | receipt; changed representation metadata; new blob and updated properties when the body changed                                                               |

Refresh applies changed allow-listed representation metadata (`Content-Type`, status, `ETag`,
`Last-Modified`) even when the body digest is unchanged. A 304 applies only supplied header fields
and retains the cached representation status; its actual response status is recorded in the receipt.
A same-body 200 applies supplied `Content-Type` and replaces its validators, clearing stale `ETag`
or `Last-Modified` values absent from the response. Metadata-only changes retain the body reference,
blob and content attachment; `changed` describes a body/reference change. A 304 with no changed
metadata writes only a receipt. The document retains the fetched body's request `Accept` and
`Accept-Language` for subsequent refresh; fetch and refresh receipts record that allow-listed
negotiation and allow-listed response headers. HEAD does not replace the cached GET body's negotiation.

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
so the fetch history of a resource is a note chain. An unchanged body retains its blob and content
attachment while changed representation metadata is updated as specified in D3; unchanged body and
metadata produce a receipt and nothing else.

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
- A4 `refresh` with unchanged body and representation metadata writes no entity or blob change;
  controls: changed body updates `blob_ref` and `content_digest`, while same-body or 304 changed
  response metadata updates supplied fields without changing the content attachment. A changed ETag
  is sent on the next conditional request, and stored Accept/Accept-Language are resent.
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

Query-key sorting means documents such as `...?add=1&mul=2` and `...?mul=2&add=1`, differing only in query parameter order, resolve to one document; for an endpoint where parameter order is significant this is an accepted loss.

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
[ADR-073](ADR-073-multi-backend-storage.md) grants no atomicity across backends and asks handlers
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

Acceptance gains three arms: after a fetch with `persist` true the entity carries one `content`
attachment and its receipt carries none; after a fetch with `persist` false no blob is stored, the
receipt carries digest and size, and neither record carries an attachment; hard-deleting a routed
entity leaves no attachment row for it on the main backend.

### A1.3 A9: attachment rows are the one web write that lands on the main backend

A9 reads "with two backends configured, web writes land in the web backend only". Under ADR-160
that is true of entity, note and edge rows and false of attachment rows by design, so A9 is amended
to: with two backends configured, a web pack's entity, note and edge rows land in the web backend
only, and its attachment rows land on the main backend only (ADR-160); the arm asserts both halves.
