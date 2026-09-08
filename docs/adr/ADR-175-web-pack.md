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
