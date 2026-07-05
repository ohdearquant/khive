# ADR-001: Entity Kind Taxonomy

**Status**: accepted\
**Date**: 2026-05-22\
**Authors**: khive maintainers

## Context

khive is a research knowledge graph runtime. Entities are the nodes — papers, algorithms,
people, tools, learned profiles, running services, generated checkpoints.

The entity kind taxonomy must satisfy competing constraints:

1. **Agent reliability**: AI agents are the primary graph writers. Fewer kinds = fewer
   misclassifications. But too few kinds and the residual bucket dominates, degrading
   query precision.
2. **Query utility**: Entity kinds are the primary filter. "Show all papers" should be a
   kind filter, not a property filter. But adding kinds that don't enable useful queries
   wastes the classification budget.
3. **Closed-world governance**: Free-form type strings drift. Agents write "algorithm",
   "Algorithm", "algo", "method" interchangeably. A governed vocabulary prevents this,
   but must remain extensible through packs.

The tension: too granular and agents misclassify (wasting graph quality); too generic and
the kind field has no discriminative power.

A top-level entity kind should exist only when it changes at least one of:

| Test                     | What it means for khive                       |
| ------------------------ | --------------------------------------------- |
| **Identity semantics**   | Durable identity independent of text content? |
| **Lifecycle semantics**  | States unlike normal research objects?        |
| **Endpoint semantics**   | Needs distinct edge endpoint rules?           |
| **Indexing/retrieval**   | Users ask for it as a primary filter?         |
| **Provenance semantics** | Derivation/versioning central to its meaning? |

Brain profiles, checkpoints, and snapshots are generated, versioned state artifacts — they
pass all five tests against existing kinds. Running inference engines and deployed APIs are
operational instances with health and endpoints — they fail the 5-test for `Project`.

Separately, free-form `properties.type` has become the de facto ontology layer despite being
ungoverned. The system appears closed (enum) but the real semantics leak into unvalidated
strings. This is the worst combination: looks governed, actually isn't.

## Decision

### 8 base entity kinds plus pack-side `resource`

`khive_types::EntityKind` remains a closed Rust enum with 8 base variants. The KG pack
validator additionally accepts `resource` as a pack-side kind for actionable content
governed by ADR-048. Harmonizing this with the shared type enum is follow-up code work,
not part of this doc-only batch.

```rust
pub enum EntityKind {
    Concept,
    Document,
    Dataset,
    Project,
    Person,
    Org,
    Artifact,
    Service,
}
```

| Kind         | What it covers                                                                  |
| ------------ | ------------------------------------------------------------------------------- |
| **Concept**  | Algorithms, techniques, architectures, theories, models, research gaps, metrics |
| **Document** | Papers, preprints, reports, blog posts, books, specifications, theses           |
| **Dataset**  | Benchmarks, corpora, evaluation sets, training sets                             |
| **Project**  | Codebases, libraries, tools, frameworks, applications, repositories             |
| **Person**   | Researchers, engineers, authors                                                 |
| **Org**      | Labs, companies, institutions, consortia, standards bodies                      |
| **Artifact** | Generated/versioned state: checkpoints, snapshots, profiles, embedding indexes  |
| **Service**  | Running operational instances: inference engines, deployed APIs, MCP servers    |

`Concept` remains the default / residual bucket.

### Governed subtype: `entity_type`

`entity_type` is a first-class field on `Entity`, validated at write time against the
`EntityTypeRegistry`.

```rust
pub struct Entity {
    pub header: Header,
    pub kind: EntityKind,
    pub entity_type: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub properties: BTreeMap<String, PropertyValue>,
    pub tags: Vec<String>,
    pub deleted_at: Option<Timestamp>,
}
```

`entity_type` replaces raw `properties.type` as the canonical subtype field.

#### Registry contract

The `EntityTypeRegistry` governs which `entity_type` values are valid for each `EntityKind`:

1. Each `(EntityKind, entity_type)` pair is a registered entry with a canonical snake_case name.
2. Aliases map to canonical names (e.g., `"algo"` → `"algorithm"`).
3. Write-time normalization: trim → lowercase → snake_case → alias resolution → validate.
4. Unknown values are rejected with an error listing valid options.
5. `entity_type` scoped by `EntityKind`: `"paper"` is valid for `Document` but not `Concept`.

#### Initial canonical subtypes

| Kind         | Canonical subtypes                                                                                                                                    |
| ------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Concept**  | `algorithm`, `technique`, `architecture`, `model_family`, `theory`, `research_gap`, `design_pattern`, `mathematical_operation`, `metric`, `objective` |
| **Document** | `paper`, `report`, `blog_post`, `book`, `specification`, `documentation`, `thesis`                                                                    |
| **Dataset**  | `benchmark`, `corpus`, `training_set`, `evaluation_set`, `test_set`, `synthetic_dataset`                                                              |
| **Project**  | `library`, `framework`, `tool`, `application`, `repository`                                                                                           |
| **Person**   | _(none — roles like researcher/engineer are metadata, not subtypes)_                                                                                  |
| **Org**      | `academic_institution`, `company`, `research_lab`, `nonprofit`, `government_agency`, `consortium`, `standards_body`                                   |
| **Artifact** | `checkpoint`, `snapshot`, `export`, `embedding_index`, `state_bundle`, `profile`                                                                      |
| **Service**  | `inference_engine`, `retrieval_engine`, `embedding_engine`, `api`, `database`, `search_engine`, `mcp_server`                                          |

#### Registry ownership

- **Runtime** owns core subtypes (the table above) and the registry data structure.
- **Packs** may register additional subtypes at boot. Each registration must specify
  `(base_kind, canonical_name, aliases)`.
- **Collision rule**: same `(base_kind, canonical_name)` from two different packs = boot error,
  unless definitions are identical and explicitly marked shared.
- **Alias collision**: an alias resolving to two different canonical names = boot error.

Pack-registered subtype examples:

| Pack  | Subtype          | Base kind  |
| ----- | ---------------- | ---------- |
| brain | `brain_profile`  | `Artifact` |
| brain | `brain_state`    | `Artifact` |
| brain | `beta_posterior` | `Artifact` |

### Pack extensibility rule

**Packs MUST NOT create new shared `khive_types::EntityKind` variants without an ADR.**
ADR-048 documents the current shipped exception: the KG pack validator includes a
pack-side `resource` kind for actionable knowledge resources, while the shared type enum
still has 8 base kinds.

Pack-declared entity kind strings MUST resolve to a closed base `EntityKind` plus a registered
`entity_type`. The `Pack::ENTITY_KINDS` string list is a subtype registration, not an enum
extension.

```text
"brain_profile" resolves to:
    kind = Artifact
    entity_type = brain_profile
```

Pack declarations should use `EntityTypeDef`:

```rust
pub struct EntityTypeDef {
    pub token: &'static str,       // "brain_profile"
    pub base_kind: EntityKind,     // EntityKind::Artifact
    pub entity_type: &'static str, // "brain_profile"
    pub aliases: &'static [&'static str],
}
```

Unresolved pack entity kind strings are a **boot error**.

### Canonical enum ownership

`EntityKind` is defined once in `khive-types/src/entity.rs`. All other crates (including
`khive-pack-kg`) re-export or reference it. No duplicate enum definitions.

### MCP verb resolution

MCP verbs accept both base kind names and registry tokens:

| Input                                         | Stored result                |
| --------------------------------------------- | ---------------------------- |
| `kind="artifact", entity_type="snapshot"`     | `Artifact` + `snapshot`      |
| `kind="brain_profile"`                        | `Artifact` + `brain_profile` |
| `kind="paper"`                                | `Document` + `paper`         |
| `kind="service", entity_type="api"`           | `Service` + `api`            |
| `kind="concept", entity_type="brain_profile"` | **reject** — wrong base kind |

The registry resolver is the single parser used by `create`, `search`, `list`, and all verbs
that accept a `kind` parameter.

## Agent Classification Heuristics

### Decision tree (evaluate in order)

```text
1. Is it a human individual?
   → Person

2. Is it an organization, lab, company, institution, team, consortium, or standards body?
   → Org

3. Is it an authored communicative work: paper, report, blog post, book, spec, thesis?
   → Document

4. Is it a curated collection of examples/records for training, evaluation, or benchmarking?
   → Dataset

5. Is it a running operational instance with endpoint, health, deployment state, or latency?
   → Service

6. Is it a codebase, library, framework, tool, application, or repository?
   → Project

7. Is it generated/versioned/materialized state: checkpoint, snapshot, profile, embedding
   index, export, or state bundle?
   → Artifact

8. Is it an abstract idea, method, algorithm, theory, architecture, research gap, or metric?
   → Concept

9. If still uncertain:
   → Concept (with entity_type if known)
```

### Signal table

| Kind         | Strong positive signals                                               | Do NOT use when                                              |
| ------------ | --------------------------------------------------------------------- | ------------------------------------------------------------ |
| **Concept**  | abstract idea, method, theory, algorithm, architecture, gap, metric   | concrete document, dataset, codebase, service, or gen. state |
| **Document** | title, authors, DOI, arXiv, publication venue, spec, report           | generated state or raw dataset                               |
| **Dataset**  | examples, records, benchmark, corpus, train/eval/test split           | vectorized/generated index or checkpoint                     |
| **Project**  | repo, package, crate, library, framework, source code, language       | running endpoint or deployed instance                        |
| **Artifact** | generated, checkpointed, exported, content-addressed, version lineage | curated example collection or authored document              |
| **Service**  | endpoint, health, latency, deployment, live process, backend          | source code project or static artifact                       |
| **Person**   | individual human                                                      | author role without standalone entity                        |
| **Org**      | lab, company, university, institution, consortium                     | project team used only as metadata                           |

### Key distinctions

**Artifact vs Dataset**: A dataset is curated examples — its identity is the records. An
artifact is generated state — its identity is the process that produced it.

| Thing                                      | Classification                 |
| ------------------------------------------ | ------------------------------ |
| MMLU benchmark questions                   | `Dataset` + `benchmark`        |
| arXiv paper corpus                         | `Dataset` + `corpus`           |
| Generated embedding index over that corpus | `Artifact` + `embedding_index` |
| Model checkpoint trained on that corpus    | `Artifact` + `checkpoint`      |
| Learned brain retrieval profile            | `Artifact` + `profile`         |

**Service vs Project**: A project is source code. A service is the running thing.

| Thing                               | Classification                 |
| ----------------------------------- | ------------------------------ |
| `qdrant/qdrant` GitHub repository   | `Project` + `application`      |
| Qdrant Cloud instance used by khive | `Service` + `search_engine`    |
| Local embedding model codebase      | `Project` + `library`          |
| Running embedding server            | `Service` + `embedding_engine` |
| MCP server binary repository        | `Project` + `tool`             |
| Running MCP server process          | `Service` + `mcp_server`       |

## Edge endpoint rules for new kinds

The following `(source, relation, target)` triples are allowed for `Artifact` and `Service`.

### Artifact

| Source     | Relation        | Target     | Meaning                                            |
| ---------- | --------------- | ---------- | -------------------------------------------------- |
| `Artifact` | `derived_from`  | `Dataset`  | generated/trained from data                        |
| `Artifact` | `derived_from`  | `Document` | generated from a document or specification         |
| `Artifact` | `derived_from`  | `Project`  | build artifact from code                           |
| `Artifact` | `derived_from`  | `Artifact` | transformation or version lineage                  |
| `Artifact` | `introduced_by` | `Document` | first described in a paper/spec/report             |
| `Artifact` | `depends_on`    | `Project`  | requires code/tooling to use or reproduce          |
| `Artifact` | `depends_on`    | `Service`  | requires running service to access or refresh      |
| `Artifact` | `instance_of`   | `Concept`  | materialized instance of architecture/model/schema |
| `Artifact` | `variant_of`    | `Artifact` | sibling variant                                    |
| `Artifact` | `precedes`      | `Artifact` | temporal ordering                                  |
| `Artifact` | `supersedes`    | `Artifact` | replacement                                        |
| `Project`  | `contains`      | `Artifact` | structurally part of the project/package           |
| `Note`     | `annotates`     | `Artifact` | notes can annotate artifacts                       |

`Artifact -[derived_from]-> X` is for material provenance. `Artifact -[instance_of]-> Concept`
is for "this checkpoint is an instance of this architecture."

### Service

| Source    | Relation      | Target     | Meaning                                    |
| --------- | ------------- | ---------- | ------------------------------------------ |
| `Service` | `instance_of` | `Project`  | deployed/running instance of this codebase |
| `Service` | `depends_on`  | `Project`  | runtime dependency on code/library         |
| `Service` | `depends_on`  | `Service`  | service-to-service dependency              |
| `Service` | `depends_on`  | `Artifact` | uses checkpoint, index, config, state      |
| `Service` | `depends_on`  | `Dataset`  | uses raw data at runtime                   |
| `Service` | `implements`  | `Concept`  | realizes an algorithm/protocol             |
| `Service` | `enables`     | `Concept`  | makes a technique/workflow possible        |
| `Service` | `precedes`    | `Service`  | earlier deployment/version                 |
| `Service` | `supersedes`  | `Service`  | replacement service                        |
| `Org`     | `contains`    | `Service`  | organization operates this service         |
| `Note`    | `annotates`   | `Service`  | notes can annotate services                |

`Service -[instance_of]-> Project` is for "deployed from" semantics.
`Service -[depends_on]-> Project` is for "requires at runtime."

## Migration

### Database

```sql
ALTER TABLE entities ADD COLUMN entity_type TEXT NULL;
CREATE INDEX idx_entities_kind_entity_type
ON entities(namespace, kind, entity_type);
```

New `entity_kind` values (`"artifact"`, `"service"`) are additive — no schema migration
beyond the new column.

### FromStr aliases for new kinds

```text
"artifact" | "art" => Artifact
"service"  | "svc" => Service
```

Subtype tokens (e.g., `"checkpoint"`, `"api"`) resolve through the registry, not `FromStr`.

### Existing data reclassification

High-confidence legacy entities should be reclassified:

| Legacy pattern                      | New classification           |
| ----------------------------------- | ---------------------------- |
| `properties.type = "brain_profile"` | `Artifact` + `brain_profile` |
| `properties.type = "checkpoint"`    | `Artifact` + `checkpoint`    |
| `properties.type = "snapshot"`      | `Artifact` + `snapshot`      |
| has endpoint + health/status        | `Service` + appropriate      |

Ambiguous cases receive migration metadata:

```text
properties["khive:migration_candidate_kind"] = "artifact"
properties["khive:previous_kind"] = "concept"
properties["khive:migration_version"] = "adr-001-v2"
```

`Concept + entity_type=brain_profile` is NOT a valid permanent state if `brain_profile` is
registered under `Artifact`.

### Forward compatibility (VCS import)

When an older khive version encounters an unknown entity kind in a snapshot import:

```text
kind = Concept
entity_type = null
properties["khive:original_kind"] = <unknown kind string>
properties["khive:original_entity_type"] = <original entity_type, if present>
tags += ["khive:degraded_kind"]
emit warning
```

### Deprecations

- `properties["type"]` is deprecated as an ontology field. Use `Entity.entity_type`.
- `properties["khive:entity_type"]` MUST NOT compete with the struct field.
- `khive:*` property keys are reserved for runtime-managed properties.

## Rationale

### Why add Artifact?

Brain profiles, checkpoints, snapshots, and embedding indexes fail the 5-test for existing
kinds:

| Test               | Concept? | Document? | Dataset? | Project? | Artifact |
| ------------------ | -------- | --------- | -------- | -------- | -------- |
| Identity semantics | no       | no        | no       | no       | **yes**  |
| Lifecycle          | no       | no        | no       | no       | **yes**  |
| Endpoint rules     | wrong    | wrong     | wrong    | wrong    | **yes**  |
| Retrieval filter   | noisy    | no        | no       | no       | **yes**  |
| Provenance         | no       | no        | partial  | no       | **yes**  |

They are generated, versioned, have derivation lineage, and no independent codebase.

### Why add Service?

Running inference engines, deployed APIs, and MCP servers fail the 5-test for `Project`:

- A project is source code; a service is a running instance with health, latency, endpoints.
- `Project` cannot carry deployment state, endpoint URLs, or availability semantics.
- "Show all running inference engines" requires `kind=Service`, not property filtering.

### Why NOT separate Model, Algorithm, Technique?

- "Is LoRA an algorithm or a technique?" — 20-30% agent misclassification rate.
- No distinct edge patterns, property schemas, or query utility.
- Governed `entity_type` handles this: `Concept` + `entity_type=algorithm`.

### Why governed entity_type instead of open properties.type?

`properties.type` as the de facto ontology is the biggest architectural risk. The system
looks governed (closed enum) but the real semantics are ungoverned (free-form strings).
This creates:

- Query noise (must handle spelling variants)
- Agent classification drift
- Unmergeable migration paths
- False appearance of governance

### Why closed enum, not pack-extensible entity kinds?

Making entity kinds pack-extensible would infect every query, validator, endpoint rule, and
serialization path with uncontrolled vocabulary.

The correct boundary:

```text
EntityKind  = closed, compile-time, query-primary, high-confidence classifier
entity_type = governed subtype, write-time normalized, pack-extensible
properties  = metadata, not ontology
```

### Future extensibility

Entity kinds are closed. A future `EntityKind::Extended(KindSpec)` mechanism would need to
demonstrate distinct identity semantics, lifecycle semantics, endpoint rules, indexing
behavior, and provenance semantics that cannot be represented as `base_kind + entity_type`.

## Consequences

### Positive

- Type-safe entity classification at compile time for 8 kinds.
- Governed subtype layer prevents free-text drift.
- Clean queries: `kind=Artifact`, `kind=Service`, `kind=Artifact AND entity_type=checkpoint`.
- Pack extensibility through subtype registration, not enum hacking.
- Agent heuristics with ordered decision tree reduce misclassification.
- Forward-compatible VCS import with degradation.

### Negative

- Breaking change: downstream crates with exhaustive `EntityKind` matches must update.
- Migration effort for existing data reclassification.
- Subtype registry adds boot-time complexity.

### Neutral

- `Concept` remains the residual bucket. With governed `entity_type`, it carries refinement
  instead of being a semantic black hole.

## Implementation

- `crates/khive-types/src/entity.rs`: `EntityKind` enum (8 variants), `Entity` struct with
  `entity_type` field. Canonical definition — all other crates reference this.
- `EntityTypeRegistry`: built at runtime boot from core subtypes + pack registrations.
- Serde: `#[serde(rename_all = "snake_case")]` for JSON interop.
- `FromStr`: accepts base kind names + short aliases (`art`, `svc`). Subtype tokens resolve
  through the registry, not `FromStr`.
- SQL: `entity_type TEXT NULL` column, indexed with `(namespace, kind, entity_type)`.
