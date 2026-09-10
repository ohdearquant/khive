# ADR-175: Web Pack — Site Ontology and Manifest Ingest for Agent-Readable Web Origins

- **Status**: Accepted
- **Date**: 2026-09-07
- **Extends**: [ADR-069](ADR-069-subject-model.md) (Subject model: OntologySpec, Scanner, Extractor,
  Layout), [ADR-085](ADR-085-code-pack.md) (the domain-ontology pack shape this record follows)
- **Depends on**: [ADR-001](ADR-001-entity-kind-taxonomy.md) (pack extensibility rule), [ADR-002](ADR-002-edge-ontology.md)
  (closed relation set, endpoint contract), [ADR-017](ADR-017-pack-standard.md) (`EDGE_RULES`,
  `ENTITY_KINDS`), [ADR-023](ADR-023-declarative-pack-format.md) (self-registration)
- **Relates to**: [ADR-072](ADR-072-subject-ontologyspec-as-data.md) (OntologySpec as data; Proposed,
  no loader in the tree), [ADR-111](ADR-111-blob-store.md) (bytes-by-pointer for later visual assets)

## Context

A growing class of web origins publishes a machine-readable description of itself for agents: a
manifest at `/.well-known/arw.json` (the ARW profile), an `llms.txt` summary, per-page markdown
"machine views" with frontmatter, and optional declarations of callable tools, agent skills and
integrations under `/.well-known/`. Today khive has no vocabulary for any of it. An agent that reads
such a site can store what it learned only as untyped `concept` and `document` entities with free-form
properties, so two agents reading the same origin produce two incompatible graphs, and nothing can be
queried across sites ("which sites expose a catalog search tool", "which pages have a machine view").

The 2026-08-21 working session settled the direction: the web-domain vocabulary is established in
khive proper, once, so that downstream tooling adds features on top of one standard instead of
carrying a second one. This record is that vocabulary and the one ingest verb that fills it.

Two shipped precedents fix the shape. The formal-math vertical (ADR-069) and the code pack (ADR-085)
are both domain-ontology packs: pack-registered entity subtypes over the closed base kinds, additive
`EDGE_RULES` over the closed 17 relations, an ingest path that transcribes a declared source into a
dedicated map database, and nothing else. The ARW manifest is itself a declared domain ontology
(site, content, tools, skills, integrations, signals), so it maps onto the ADR-069 Subject with no new
architecture: an OntologySpec (this record's D2 and D3), a Scanner (manifest and views to raw
declarations, D4), an Extractor (raw declarations to one entity/edge batch, D4), and Layout (taxonomy
from the site's own declared domain and tags, D2).

ADR-072 would express the vocabulary as a runtime-loaded data file instead of a crate. It is Proposed
and has no loader in the tree, so this record does not wait on it; D6.5 keeps the vocabulary pure data
so the migration is a transcription when 072 lands.

## Decision

### D1: Scope — a domain-ontology pack with one ingest verb

`khive-pack-web` models what an agent-readable web origin declares about itself as typed graph
vocabulary. It contributes:

- five entity subtypes registered in the entity type registry (D2),
- additive `EDGE_RULES` making the site triples legal (D3),
- one verb, `web.ingest`, that reads a site tree and writes a dedicated map database (D4),
- nothing else: no crawler, no fetcher in v0, no quality scoring, no rendering, no background service.

Protocols the site implements (ARW, MCP, UCP, ACP) are the existing `interface` concept subtype
(alias `protocol`, registered by ADR-085) and are not re-declared. Organizations behind a site remain
base `org` entities. No new base entity kind, no new note kind, and no new edge relation is introduced.

### D2: Five subtypes, registered in the entity type registry

| Token          | Base kind  | Aliases          | Meaning                                                                                                                             |
| -------------- | ---------- | ---------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `site`         | `Service`  | `origin`         | One web origin publishing a manifest; properties carry `homepage`, `contact`, `profile`, `content_signals`                          |
| `page`         | `Document` | `web_page`       | One declared content entry: `url`, `description`, `tags`, `purpose`, `priority`, `authored`, `chunks`                               |
| `machine_view` | `Document` | `view`           | The markdown rendering a page declares at `markdown_url`; frontmatter fields (`page_type`, `schema_org_type`, `aeo`) are properties |
| `agent_tool`   | `Service`  | `mcp_tool`       | One callable the site declares for agents (name, description, input schema reference); a served endpoint, so a service              |
| `agent_skill`  | `Document` | `skill_manifest` | One declared agent skill (name, description, the tools it names); a served instruction file, so a document                          |

The tokens `tool` and `skill` are not claimed: `tool` is an existing `project` subtype and both words
are existing aliases of the kg pack's local `resource` kind, so the web vocabulary takes qualified
tokens. `resource` itself is not a shared `EntityKind` (the shared enum has eight variants, ending at
`Service`), and the subtype registry and validated create path parse the shared enum, so no web subtype
is assigned to it: a tool is a served endpoint (`Service`), a skill is a served instruction file
(`Document`). Tokens
are added to the registry in `crates/khive-types/src/entity_type.rs` following the ADR-085 precedent;
the consequence that subtype tokens validate everywhere while edge rules apply only where the pack is
loaded is inherited unchanged.

Layout (ADR-069 taxonomy configuration): discipline is the view's declared `aeo.domain` when present
(for example `ecommerce`), subdiscipline is the page's declared `tags`. Both are transcribed, never
inferred. `content_signals`, `aeo.quality_score` and `reading_ease` are stored as properties exactly
as declared; khive computes none of them.

Deliberately excluded from v0 (amend this record with evidence before adding): `chunk` (a chunk is a
heading anchor inside a page and is carried as the page's `chunks` property list), `integration`,
`api_endpoint`, `commerce_offer`, visual assets (screenshots ride ADR-111 when a consumer exists),
and any cross-site reference relation.

### D3: Additive edge rules

All rules bind `EndpointKind::EntityOfType { kind, entity_type }` for subtypes, never bare kinds, per
ADR-069 D3. Every relation is one of the closed 17; no native label is needed against the manifests
this record was written from (every declared relation is containment, derivation, dependency or
implementation).

| # | Relation       | Source                  | Target                 | Reading                                   |
| - | -------------- | ----------------------- | ---------------------- | ----------------------------------------- |
| 1 | `contains`     | service/`site`          | document/`page`        | site declares page                        |
| 2 | `contains`     | service/`site`          | service/`agent_tool`   | site exposes tool                         |
| 3 | `contains`     | service/`site`          | document/`agent_skill` | site publishes skill                      |
| 4 | `derived_from` | document/`machine_view` | document/`page`        | view is rendered from page (base-covered) |
| 5 | `depends_on`   | document/`agent_skill`  | service/`agent_tool`   | skill names tool                          |
| 6 | `implements`   | service/`site`          | concept/`interface`    | site implements protocol (base-covered)   |

Rows 4 and 6 are already legal under the base endpoint contract (`document derived_from document`,
`service implements concept`) and are declared for introspection only, the practice the code pack
follows for its base-covered rows; rows 1, 2, 3 and 5 are the additive ones. `part_of` in the reverse
direction is not declared; `contains` is the canonical direction, as in
ADR-085. `instance_of` (tool to tool type) waits for a tool taxonomy and is not v0. Cross-site
references (`mentions`) wait on the ADR-002 ratification of that label and carry no rule here.

Relied-on base triples, listed for implementers: `service introduced_by org|person`, `* annotates *`
for observations about a site, the epistemic rails `{document,artifact} supports|refutes concept`.

### D4: `web.ingest` — Scanner and Extractor over a site tree

One verb, `web.ingest(source, db?, include_views?)`, commissive, opt-in:

- **Source (v0)**: a local directory holding the served tree of one origin (the files the origin
  serves at `/.well-known/arw.json`, `/llms.txt`, and each declared `markdown_url`). A live `https`
  origin is not a v0 source: fetching from the daemon is outbound egress and rides the gate and mount
  decisions of ADR-142, so it is a follow-up amendment, not this record.
- **Scanner** reads, in order: `.well-known/arw.json` (required; absent or unparsable refuses the
  whole site with a classified `manifest_missing` or `manifest_malformed` error), `llms.txt`
  (optional; its embedded yaml block is cross-checked against the manifest, and a disagreement is a
  quarantined declaration with the manifest winning), and each declared view's frontmatter (optional;
  a missing view yields a `page` with no `machine_view`). Not read in v0: HTML pages, `sitemap.xml`,
  `robots.txt`, the bodies under `.well-known/ucp`, `acp.json`, `mcp` and `agent-skills` beyond
  presence and the tool and skill declarations the manifest carries.
- **Extractor** maps raw declarations to one entity/edge batch with deterministic ids: UUIDv5 under a
  pack namespace constant keyed by `(origin, url)` for pages and views, `(origin, tool name)` and
  `(origin, skill name)` for tools and skills, `(origin)` for the site. Canonicalization of the key:
  origin is the manifest's `site.homepage` host, lowercased, scheme and port dropped; url is the
  declared path with one leading slash and no trailing slash; names are the declared strings as-is.
  Re-ingesting the same tree is idempotent; a changed declaration updates the same row.
- **`include_views`** defaults to true; false skips the view files entirely (no `machine_view` rows, no
  rule-4 edges) and the report says so. A declared view that is absent on disk yields the page without
  a view and one `views_missing` count in the report, not a quarantine.
- **Target**: a dedicated map database, default `<source>/.khive/web-map.db`; the shared production
  database is always rejected, with no override (ADR-085 D6.1, mirrored).
- **Report**: counts per subtype and relation, `views_missing`, quarantined declarations with reason,
  the manifest digest (BLAKE3 of the manifest file's raw bytes, hex), and the source path. The report
  is the verb's result, never a side file.

### D5: Pack mechanics

Following `khive-pack-code`'s shipped shape: crate `crates/khive-pack-web`, `NAME = "web"`,
`REQUIRES = ["kg"]`, `ENTITY_KINDS = []` (tokens live in the registry, D2), `HANDLERS` = the D4 verb,
`NOTE_KINDS = []`, `EDGE_RULES` = the D3 table, `SCHEMA_PLAN = None`. Self-registration via
`inventory::submit!` plus the anchor import in the binaries (ADR-023). Not in the default pack set
at v0; loaded opt-in via `KHIVE_PACKS=kg,web` / `--pack kg --pack web` (the pack requires `kg`).
Promotion to the default set is a
follow-up decision gated on one validated real ingest of a multi-site tree.

### D6: Hard constraints

1. **Granularity fence.** Whole-fleet ingests target dedicated map databases through `web.ingest`;
   the shared production graph receives only curated site entities an agent references in its work.
2. **Transcribe, do not invent** (ADR-069 D5). Names, descriptions, tags, signals and scores are the
   declared values as-is or omitted; nothing is synthesized, scored or summarized by the pack.
3. **Registry-valid tokens only.** Ingest writes canonical subtype tokens through the validated create
   path; rules match `(kind, entity_type)`.
4. **No secrets, no egress.** v0 reads local files only; credentials for a private origin are never
   part of the verb's arguments or the report.
5. **ADR-072 forward compatibility.** D2 tokens and D3 rules are pure data with no logic entangled;
   if ADR-072 ships its loader, the vocabulary migrates verbatim to a web OntologySpec and the crate
   shrinks to the verb.
6. **Fixtures are synthetic.** Test fixtures use fictional site trees only; no real brand's content
   is committed to this repository.

## Acceptance, stated before implementation

Each arm names its expected red before it runs; the mutations are the controls that prove the arm is
load-bearing.

1. **Fixture counts.** Two fictional site trees ingest to exact, pre-stated counts per subtype and per
   relation in the report (tree A: tools, skills and views; tree B: manifest-only, so zero tools, zero
   skills, pages without `machine_view`).
2. **Manifest refusal.** A missing manifest refuses the whole site with `manifest_missing`; a malformed
   one with `manifest_malformed`; in both cases the map database holds zero rows afterwards.
3. **Cross-check.** An `llms.txt` disagreeing with the manifest yields one quarantined declaration
   naming the field and the reason; the manifest value is what is stored.
4. **Idempotence.** Re-ingesting the same tree changes nothing (row count and ids equal). One changed
   declaration updates the same id and the count is unchanged. Mutation: drop the UUIDv5 keying and the
   re-ingest doubles the rows (red).
5. **Production refusal.** `db` pointed at the production database is refused before any write, with
   no override. Mutation: remove the check (red).
6. **Views switch.** `include_views=false` on tree A produces zero `machine_view` rows and zero rule-4
   edges with the page counts unchanged; a declared view removed from disk yields `views_missing` of one
   and no quarantine.
7. **Vocabulary.** Every D2 token validates through the create path; the endpoint-rule introspection
   test lists the six D3 rules as declared, asserts rows 1, 2, 3 and 5 are absent from the base contract
   (additive), and accepts rows 4 and 6 as intentional restatements of base rows.

## Rationale

**Why a domain-ontology pack and not a capability pack.** The value is a shared vocabulary that two
readers of the same origin fill identically; the graph verbs already exist. A pack that also crawled,
fetched or scored would carry policy (egress, freshness, quality) that belongs to the caller and to
the gate, and it would make the ontology hostage to that policy's review.

**Why one verb instead of zero.** ADR-085 v0 shipped with zero verbs and added `code.ingest` in its
second amendment because hand-transcribing a declared source is where every reader diverges. The
manifest is machine-readable by construction; the deterministic mapper is the standard.

**Why qualified tokens.** `tool` and `skill` already mean something in the registry and in the bare
kind aliases; overloading them would make `kind=tool` answer two questions.

**Why a local tree before a live origin.** The local tree is the same bytes the origin serves, with
no egress, no credentials and a deterministic fixture. The fetch layer is a separate decision with
its own gate, and putting it first would hold the vocabulary behind that review.

## Alternatives Considered

- **A1: OntologySpec data file under ADR-072.** Rejected for v0: ADR-072 is Proposed with no loader;
  a vocabulary nobody can load ships nothing. D6.5 keeps the migration a transcription.
- **A2: Untyped `document`/`concept` entities with a `web` property namespace.** Rejected: the
  endpoint contract cannot fire on properties, so `contains`/`derived_from` between pages and views
  would be either illegal or unvalidated.
- **A3: A `web_page` note kind instead of a `page` entity.** Rejected: pages are structural and
  addressable across sites; notes are epistemic and per-observer.
- **A4: New base kinds (`site`, `tool`).** Rejected by ADR-001's extensibility rule; subtypes suffice.
- **A5: Build the fetcher first.** Rejected; see Rationale.

## Consequences

- A second reader of the same origin produces the same map rows, byte for byte on the deterministic
  ids, so cross-site queries are meaningful.
- Downstream tooling gains a standard to build on instead of a second vocabulary to reconcile.
- The registry gains five tokens that validate in every deployment; the rules apply only where the
  pack is loaded (inherited asymmetry, ADR-085 D2).
- A live-origin source, a tool taxonomy, chunk entities, integrations and visual assets are each a
  separate amendment with its own evidence.

## Open Questions

1. Whether `machine_view` deserves its own subtype or should be a property of `page` once views are
   universal in the profile; v0 keeps the subtype because a view carries its own frontmatter fields.
2. Whether `agent_skill` belongs under `document` or `concept`; v0 chooses `document` because a skill
   is declared as a served instruction file the agent loads, not an idea.

## Implementation

1. The five D2 entries added to the shared registry in `crates/khive-types/src/entity_type.rs` (the
   ADR-085 path, so they validate through the shared create path); the D3 `EDGE_RULES` in
   `crates/khive-pack-web/src/vocab.rs` with the endpoint-rule introspection test the code pack carries.
2. Scanner (`manifest.rs`, `views.rs`) and Extractor (`extract.rs`) with the deterministic id
   namespace; `web.ingest` handler; the map-database refusal of the production path as a web-specific
   wrapper (the code pack's fence hard-codes its own default filename and verb label, so the safety
   comparison is reused and the default and error text are the web pack's own).
3. Fixtures: two fictional site trees (one with tools, skills and views; one manifest-only) and a
   broken manifest. The Acceptance section above is the authoritative test matrix: one test per arm,
   plus the two mutations.
4. `docs/packs/web.md`: the vocabulary tables, the report shape, and a ten-line multi-site run.
5. Status flip to Accepted by the rule in the catalog README once the implementation lands.

## References

- ADR-069 Subject model; ADR-072 OntologySpec as data; ADR-085 code pack and its amendments
- ARW profile: manifest at `/.well-known/arw.json`, `llms.txt`, markdown machine views

## Amendment 1 (2026-09-09): `web.fetch` and `web.search`, and the egress constraint they reverse

D1 says "no crawler, no fetcher in v0" and D6.4 says "no secrets, no egress; v0 reads local files
only". This amendment reverses the egress half of both, deliberately and with the constraints that
made them worth writing carried forward rather than dropped. It does not introduce a crawler: both
verbs are single-request reads a caller asks for by name, with no link following and no queue.

The reason for the reversal is that an agent's read of the outside world is happening either way.
Today it happens through whatever the harness hands the model, outside khive, which means no
allowlist, no receipt, and no record of what text entered a decision. Moving it into a pack is what
makes it bounded and auditable.

### A1.1 `web.fetch(url, method, headers, credential, max_bytes, timeout_s, namespace)`

An HTTP read of one URL. `GET` and `HEAD` only; a body-bearing method is refused, because a verb
that can POST is a verb that can act on the world and this one is a read.

The reply is `{final_url, status, headers, content_ref, bytes, truncated, redirects, receipt_id}`.
For `GET`, the body goes to the blob store and the reply carries its reference, never the bytes: a
page is routinely larger than a response envelope should be, and a caller that wants the text reads
it with `blob.get` like any other stored object. `HEAD` reads no body and writes no blob; its reply
has `content_ref: null`, `bytes: 0` and `truncated: false`, regardless of a response's advertised
content length. The reply's `headers` is an allow-listed subset (content type, content length, last
modified, etag); a response header set is attacker-controlled and echoing it whole puts attacker text
in a place callers read structurally. The request `headers` argument is allow-listed too, for the
reason in A1.2.7.

### A1.2 What the fetch refuses, and where

1. **Scheme and userinfo.** `http` and `https` only. Everything else refuses, including `file`,
   `ftp` and `data`. A URL containing userinfo, including `user:password@host`, refuses before any
   outbound request; credentials enter only through rule 6. These checks also apply to redirect
   targets. With a credential in play, rule 6 additionally requires `https` at every hop.
2. **Address, after resolution.** The host is resolved and every returned address is checked; a
   loopback, link-local, private, unique-local, multicast, broadcast or unspecified address refuses;
   the shared address space 100.64.0.0/10 counts as private. The check is on the
   resolved address rather than on the hostname, because a name that resolves into private space is
   the whole shape of the attack, and it is re-applied on every redirect hop rather than once. The
   connection is then made to an address that passed, never to a fresh resolution of the name: a
   resolver that answers differently on the second call walks straight through a check that passed,
   because the name is the same and the check is the same and only the address is different. Either
   pin the connection to the checked address, or read the peer address after connect and refuse on
   mismatch.
3. **Operator allowlist, when set.** With no allowlist configured, the public internet is reachable
   subject to the other rules in this section. When an allowlist is configured it becomes exclusive,
   and a host outside it refuses with the host named.
4. **Redirects.** Bounded, default five, with every hop re-checked against rules 1 through 3 and the
   credential constraints in rule 6. A redirect into private address space refuses at the hop that
   proposes it.
5. **Size and time.** Operator configuration sets finite defaults and ceilings for `max_bytes` and
   `timeout_s`; a caller may lower either bound but cannot raise it above its operator ceiling.
   An argument above a ceiling refuses before any outbound request, naming the parameter and its
   ceiling. Omitted arguments use the configured defaults, which must not exceed their ceilings.
   The concrete values are implementation configuration; this ceiling rule is the contract.
   A response exceeding the byte bound is stored truncated with `truncated: true` rather than
   discarded, so a caller sees what was read; a response exceeding the time bound refuses and stores
   no body object. The time bound covers the whole read, including redirects and decompression.
   The byte bound is on decompressed bytes, because a small compressed response can expand without
   limit and a bound on the encoded stream bounds nothing the caller ever sees; decoding stops at
   the bound and the result is stored truncated like any other over-long response.
6. **Credentials are never arguments, and each is bound to a host set.** `credential` names an entry
   the operator has configured; the value is read from the process environment at request time. A
   secret in the verb's arguments would be in the receipt, the audit event, and every log that carries
   either. Every configured credential also carries the set of hosts it may be presented to (exact
   hosts or suffixes, operator configuration), because a secret bound only to a name is presented to
   whatever host the caller names. Host comparisons are lowercase with a trailing dot stripped from
   both the URL host and configured DNS names; the port is not part of the host set. A suffix entry
   `example.com` matches `example.com` or a name ending in `.example.com`, at a DNS label boundary
   only, never `evilexample.com`. IP literals are exact-address entries only, never suffix matches.
   A request naming a credential for a host outside its set refuses before any request is made,
   naming the credential and the host. On a redirect whose next hop is outside the set the hop
   refuses; the credential is never sent to the hop, and rule 4's address re-check does not stand in
   for this one, since it checks address class and not credential scope. A credential-bearing read
   also requires `https` at every hop: an initial `http` URL or an `https` to `http` redirect refuses
   before requesting that hop, even when its host is in the credential's set. Stripping the
   credential and continuing over `http` is not an alternative to refusal.
7. **Request headers, allow-listed, with the credential-bearing ones refused by name.** `headers`
   accepts `Accept`, `Accept-Language`, `If-None-Match`, `If-Modified-Since` and `User-Agent`; any
   other header refuses with the header named. `Authorization`, `Cookie` and `Proxy-Authorization`
   refuse with their own reason, which names `credential` as the only path a secret takes. Without
   this rule refusal 6 protects one parameter while the one beside it is open: a caller passing
   `Authorization: Bearer <secret>` puts that secret in the receipt, the audit event and every log
   that carries either, which is the exact harm 6 exists to prevent, with the credential mechanism
   routed around rather than defeated.

### A1.3 `web.search(query, limit, provider, max_bytes, timeout_s, namespace)`

A query against a configured search provider, returning `{provider, results: [{title, url, snippet}],
receipt_id}`. The provider is operator configuration, not a verb argument beyond selecting among the
configured ones. With no provider configured the verb refuses with `no_search_provider_configured`
and names what to configure; it does not return an empty result list, because an empty list from a
missing provider is indistinguishable from an empty list for a query with no hits, and a caller
cannot act on the difference it cannot see.

Search has the same operator-controlled timeout and decompressed response-byte bounds and ceilings
as A1.2.5, plus a finite operator default and ceiling for `limit`. Omitted values use defaults within
the ceilings; a caller may lower them. Any caller value above its ceiling refuses before the
provider request, naming the parameter and the ceiling. A provider response that exceeds the time
or byte bound refuses without storing or returning partial results; a truncated provider payload
cannot be treated as a complete result list. The concrete defaults and ceilings are implementation
configuration.

Results are transcribed as the provider returned them, per D6.2, up to the requested `limit`, in
provider order. The pack does not rank, merge, deduplicate or summarize.

### A1.4 What a fetched byte is

Every response body and every search snippet is **data, never instruction**. Both verb descriptions
say so, so a caller reading `help=true` sees it before the first call rather than in a document it
may not have. The receipt is what makes this checkable after the fact:

- A `GET` receipt records the requested URL, the final URL, the status, the content digest and the
  byte count, so a later reader can ask which text entered a decision and verify it has not changed
  underneath the reference.
- A `HEAD` receipt records the requested URL, the final URL, the status and the same allow-listed
  response-header subset as the reply, with `content_ref: null`, `bytes: 0` and `truncated: false`;
  there is no body digest or blob write.
- A search receipt records the query, the selected provider, the effective `limit`, and the ordered
  `results` returned to the caller, with a BLAKE3 digest of the exact UTF-8 JSON bytes used to serialize
  that `results` array in the reply. The receipt preserves those bytes as the digest input as well
  as the structured results, including for an empty array; it does not reconstruct a different
  ordering or a summary.

For a `GET` body, the blob put completes before the receipt write; if the put fails, no success
receipt is written. If the blob put succeeds but the receipt write fails, the verb returns an error
naming the stored `content_ref` to the calling actor, so stored bytes are not silently orphaned.
That reference is not a success receipt and remains subject to ADR-111's ref-leak hygiene. A
receipt-write failure for `HEAD` or search also returns an error, with no fabricated body reference.
The pack never retries an outbound request, including after a transport, blob or receipt failure;
the bounded redirects in A1.2.4 are separate hops, not retries of a failed request.

Fetched objects use the ordinary blob put/read path under the request's namespace attribution;
the object named by `content_ref` is retrieved through `blob.get`. ADR-111's capability-by-hash model
for local use and its mandatory gate-level `(namespace, ContentRef)` put-ledger for hosted tenants
apply exactly as they do to `blob.put`, including recording the successful put when a later receipt
write fails.
There is no web-specific blob access bypass.

Both verbs are `Assertive` and write a receipt. Neither is admission degrade-safe: a receipt is a
write, and an outbound request has an effect on the outside world (it is observed by the origin or
provider) even when it requests only a read.

### A1.5 What does not change

D6.1, D6.2, D6.3, D6.5 and D6.6 stand as written. `web.ingest` is untouched and remains local-only.
No new entity kind, note kind or edge relation is introduced; neither verb writes the graph.

Open item: credential-name binding to the caller or tenant through the Gate and TLS peer pinning
are deferred to a cloud ADR; the OSS local operator is the tenant for this amendment.

Acceptance arms, continuing the base numbering (the base ADR's acceptance ends at 7), stated before
implementation; the suite reaches no real network per D6.6, so the resolver and the listener in every
arm are fixture stubs and the public hostnames are stub-resolved names:

8. A `file://` URL refuses and stores nothing.
9. A hostname resolving to loopback refuses naming the resolved address, with a public hostname as
   a positive control in the same test.
10. A redirect chain whose second hop points into private address space refuses at that hop, with a
    same-length public chain as control.
11. A response larger than the byte bound is stored truncated with `truncated: true` and its digest
    matches the stored prefix.
12. A response slower than the time bound refuses and the blob store holds no new object.
13. A configured allowlist makes a host outside it refuse while a host inside it succeeds, both in
    one test.
14. A `credential` name that is not configured refuses without the request being made, proved by a
    request counter that does not move.
15. `web.search` with no provider configured refuses with its own reason rather than an empty result
    list.
16. A fetch receipt records the requested URL, the final URL after redirects, the status, the byte
    count and the content digest, and the digest matches what `blob.get` returns for the reference
    in the reply.
17. A fetch whose `headers` carries `Authorization` refuses naming `credential` without the request
    being made, proved by a request counter that does not move, with an allow-listed header
    succeeding in the same test as the control.
18. A host whose resolution changes between the check and the connect refuses, with a
    stable-resolution host as the positive control in the same test.
19. A compressed response whose decompressed size exceeds the byte bound is stored truncated with
    `truncated: true`, and the stored object is no larger than the bound.
20. A request naming a credential for a host outside its configured set refuses naming the credential
    and the host, proved by a request counter that does not move, with the same credential to a host
    inside the set succeeding as the control.
21. A redirect whose second hop is outside the credential's set refuses at that hop, and the counter
    shows the first hop was made and the second was not.
22. With a credential in play, an initial `http` URL refuses with a zero request counter; an `https`
    to `http` redirect within the credential's host set refuses with counters of one for the first
    hop and zero for the second, with an all-`https` chain in that set succeeding as the control.
23. A credential scoped to suffix `example.com` refuses `evilexample.com` without a request, while
    `api.example.com` succeeds in the same test; mixed case, a trailing dot and a different port
    preserve that decision. An exact IP entry permits only that address and cannot act as a suffix,
    with another public IP refusing before a request.
24. A URL with userinfo refuses and the request counter does not move, with the same URL without
    userinfo succeeding as control; a redirect to userinfo refuses before requesting that hop.
25. For fetch and search, each of `max_bytes` and `timeout_s` above its configured ceiling refuses
    naming the parameter and ceiling with a zero outbound request counter; search `limit` does the
    same. Values equal to and below each ceiling succeed, and omitted values use the operator
    defaults, all against bounded fixture responses in the same tests.
26. Search refuses an over-time response and an over-byte decompressed response without storing or
    returning partial results, with an in-bound response succeeding as control.
27. A search receipt preserves the query, selected provider, effective limit and ordered result
    array returned to the caller; recomputing BLAKE3 from its preserved serialized array bytes
    matches the digest, and those bytes match the reply's array bytes. Duplicate results and a
    nonalphabetical order survive unchanged, including when the result list is limited; an empty
    result list is also recorded with its matching digest.
28. A `HEAD` response advertising a nonzero content length yields `content_ref: null`, `bytes: 0`
    and `truncated: false`; body-read and blob-put counters stay zero. The receipt and reply carry
    the status and identical allow-listed header subsets, with an unlisted fixture header omitted
    from both. A `GET` control reads and stores its body.
29. A `GET` effect trace places the successful blob put before the receipt write; an injected put
    failure prevents a success receipt, and an injected receipt failure returns an error naming the
    stored reference, whose bytes can still be read through `blob.get`. Transport, put and receipt
    failures do not cause outbound retries: in each single-hop fixture the request-attempt counter
    stays at one. `HEAD` and search receipt failures also return errors without a body reference or
    an outbound retry, with successful receipt writes as controls.
30. A fetched object's put follows the same namespace attribution and ADR-111 access policy as an
    ordinary `blob.put` control: local reads use the reference, and the hosted gate permits the
    tenant with the put-ledger entry and refuses an ungranted tenant, even when the receipt write
    subsequently fails.
