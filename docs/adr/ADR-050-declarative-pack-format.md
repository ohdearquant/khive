# ADR-050: Declarative Pack Format and Local Pack Management

**Status**: proposed\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

[ADR-025](ADR-025-pack-standard.md) introduced the `Pack` trait as the composition mechanism for
vocabulary extension. Packs are currently Rust crates compiled into the binary (`khive-pack-kg`,
`khive-pack-gtd`, `khive-pack-memory`). This works for built-in vocabulary but has three
constraints that block third-party pack authorship and the cloud marketplace:

1. **Recompilation required for every pack change.** A researcher who wants vocabulary for ML
   experiments (`model`, `benchmark`, `training_run`) must fork khive, write Rust, compile a
   binary, and redistribute it. There is no path for vocabulary extension that does not require
   Rust development.

2. **No tool-independent validation.** The pack contract — what entity kinds, note kinds, edge
   endpoint rules, and property schemas a pack provides — lives in Rust `const` items. A
   third-party tool that wants to check whether a `pack.yaml` is valid has no specification to
   validate against that does not require building the binary.

3. **Cloud marketplace has no OSS foundation.** [ADR-033 (cloud marketplace)](https://github.com/khive-ai/cloud-adr/ADR-033) describes
   a registry, publishing, and social features for khive packs. That layer cannot exist without a
   stable, language-neutral pack format that OSS tools can read and validate. Making the format
   cloud-only would prevent local development workflows.

This ADR defines:

1. The declarative pack manifest format (`pack.yaml`)
2. How packs are declared in `schema.yaml`, extending [ADR-048](ADR-048-git-native-kg-versioning.md)
3. How the runtime merges declarative pack vocabularies with built-in Rust packs
4. Local CLI commands for pack management (`khive pack *`)
5. How built-in Rust packs and declarative YAML packs coexist

The cloud registry, publishing pipeline, and social features are deferred to the cloud marketplace
ADR (ADR-033). This ADR is the OSS foundation on which ADR-033 builds.

### Scope boundaries

This ADR covers the **format and local lifecycle** of declarative packs: what a pack declares,
how it is installed into a project, how the runtime loads it, and how the CLI manages it. It does
not cover:

- Registry authentication, publishing, or search (cloud ADR-033)
- Pack versioning constraints between packs (inter-pack semver; future work)
- Dynamic loading of pack code (packs are vocabulary, not arbitrary code; Rust packs remain
  the extension point for custom verb handlers)

## Decision

### 1. Pack manifest format

A declarative pack is a directory containing a `pack.yaml` file. The manifest is the complete
machine-readable declaration of the pack's vocabulary contribution.

```yaml
name: ml-papers
version: "1.0.0"
description: "Vocabulary for ML research paper knowledge graphs"
author: ocean
license: Apache-2.0
homepage: https://github.com/ocean/khive-pack-ml-papers

entity_kinds:
  - model
  - benchmark
  - training_run

note_kinds:
  - experiment_log

edge_endpoints:
  - relation: depends_on
    endpoints:
      - [model, dataset]
      - [model, benchmark]

properties:
  model:
    - key: architecture
      values: [transformer, rnn, cnn, diffusion, hybrid]
    - key: parameter_count
    - key: context_length
  benchmark:
    - key: metric
      values: [accuracy, f1, bleu, rouge, perplexity]
    - key: dataset_name
```

#### Name constraints

`name` must match `^[a-z][a-z0-9-]{0,62}$` — lowercase ASCII letters, digits, and hyphens, starting
with a letter, maximum 63 characters. This constraint ensures the name is safe as:

- A filesystem directory name (no slashes, no dots, no uppercase path confusion)
- A URL segment in the registry API
- An identifier in `schema.yaml` without quoting

#### Version

`version` is a semver string (`MAJOR.MINOR.PATCH`, no pre-release suffix in the registry). The
runtime does not enforce semver ordering beyond format validation — pack selection by version is
a registry concern (ADR-033), not a local runtime concern.

#### `entity_kinds` and `note_kinds`

These are additive to the base vocabulary defined by loaded Rust packs. The rules are:

- A declarative pack **cannot** redeclare a kind already registered by a loaded Rust pack or by
  another declarative pack. Attempting to do so is not an error — duplicate registration is
  idempotent in the merged vocabulary, consistent with [ADR-025](ADR-025-pack-standard.md)
  §Runtime vocabulary merging.
- Kind strings must match `^[a-z][a-z0-9_]{0,62}$` — lowercase, underscores, no hyphens.

#### `edge_endpoints`

The `edge_endpoints` section declares new `(source_kind, target_kind)` endpoint pairs for
**existing** base relations from [ADR-002](ADR-002-edge-ontology.md). Packs extend which
kinds may participate in a relation — they do not introduce new relation names.

Each entry specifies:

- `relation`: one of the 13 base relation names from ADR-002 (e.g., `depends_on`, `implements`,
  `instance_of`). This field is a **lookup into the closed relation set** — it is not a
  declaration of a new relation.
- `endpoints`: a list of `[source_kind, target_kind]` pairs that become valid for this relation.
  The source and target kinds must be registered in the merged vocabulary (either as base kinds
  or as kinds declared by this pack or a previously loaded pack).

Declarative packs **cannot** introduce new relation names. The 13-relation closed enum in ADR-002
is the complete and immutable set of relation types. If a domain requires a semantic distinction
that the 13 relations do not cover, that is a request for an ADR-002 amendment — not something
a pack author can resolve unilaterally.

Rules are additive only. A declarative pack cannot tighten or remove any endpoint pair accepted
by a base relation or a previously loaded pack.

#### `properties`

Per-kind property schema declarations. Each entry has:

- `key`: the property key name (string, must match `^[a-z][a-z0-9_]{0,62}$`)
- `values` (optional): allowed value set. If absent, any string value is accepted. If present,
  the runtime rejects writes with values not in the set.

Property declarations merge across packs: if two packs declare different `values` sets for the
same key on the same kind, the merged set is the union. A pack cannot restrict an existing
property's value set — only broaden it.

The `properties` section is informational for the runtime in v0.3 (used for validation and schema
display) but does not enforce property presence. Required properties are a future extension.

### 2. Schema.yaml integration

`schema.yaml` (introduced in [ADR-048](ADR-048-git-native-kg-versioning.md)) gains a top-level
`packs` section that declares which packs are installed in this project:

```yaml
format_version: "1.1.0"
ontology_version: "1.0.0"

packs:
  - name: kg
    version: "1.0.0"
    source: builtin # "builtin" | "registry" | "local" | "git"
  - name: gtd
    version: "1.0.0"
    source: builtin
  - name: ml-papers
    version: "1.0.0"
    source: registry

entity_kinds:
  # merged from all packs + any project-local additions
  - concept
  - document
  - dataset
  - project
  - person
  - org
  - model # contributed by ml-papers
  - benchmark # contributed by ml-papers
  - training_run # contributed by ml-papers

note_kinds:
  - observation
  - insight
  - question
  - decision
  - reference
  - task # contributed by gtd
  - experiment_log # contributed by ml-papers

# edge endpoint triples and properties are similarly merged;
# edge_relations itself does not gain new names — only new (relation, source_kind, target_kind) triples
```

The `packs` section is the **source of truth** for which vocabularies are active. The
`entity_kinds`, `note_kinds`, and `edge_relations` sections are the **merged result** — written
by `khive pack install` and `khive pack remove` so that `schema.yaml` remains self-contained for
validation even in environments where packs cannot be fetched.

#### Source values

| Source     | Meaning                                                                                               |
| ---------- | ----------------------------------------------------------------------------------------------------- |
| `builtin`  | A Rust pack compiled into the binary. Vocabulary is registered at binary start. No local file needed. |
| `registry` | Fetched from the cloud registry (ADR-033) and cached at `~/.khive/packs/<name>/<version>/pack.yaml`.  |
| `local`    | Loaded from a relative path. Used for development and monorepo packs.                                 |
| `git`      | Loaded from a git repository at a pinned commit SHA. Same SHA-pin discipline as ADR-048 remotes.      |

#### `local` source path

For `source: local`, a `path` key is required:

```yaml
packs:
  - name: my-org-pack
    version: "0.1.0"
    source: local
    path: "./packs/my-org-pack"
```

The path is relative to the `.khive/` directory. The `pack.yaml` inside that path is loaded
directly.

#### `git` source

For `source: git`, `repo` and `commit` keys are required:

```yaml
packs:
  - name: ml-papers
    version: "1.0.0"
    source: git
    repo: ocean/khive-pack-ml-papers
    commit: a1b2c3d4e5f6789012345678901234567890abcd # full 40-char SHA
```

The same SHA-pin discipline as ADR-048 remotes applies: tags and branch names are accepted on
input to `khive pack install` but resolved to full SHAs before writing to `schema.yaml`. The
stored value is always the SHA.

### 3. Vocabulary merging rules

When the runtime loads packs, it builds the merged vocabulary in a fixed order:

1. **Rust built-in packs** (always loaded first, in registration order based on `--pack` flags)
2. **Declarative packs from `schema.yaml#packs`** (loaded after built-ins, in declaration order)

Merging rules:

- **Entity kinds**: union across all packs. Duplicate kind strings are idempotent.
- **Note kinds**: union across all packs. Duplicate kind strings are idempotent.
- **Edge endpoint rules**: additive union. New `(relation, source_kind, target_kind)` triples are
  added to the runtime's edge rule set (consistent with ADR-031). Duplicates are idempotent.
  The `relation` in each triple must be one of the 13 base relations from ADR-002; the runtime
  rejects any `edge_endpoints` entry whose `relation` field does not match a known base relation.
- **Properties**: per-kind key merge. Duplicate property keys have their `values` sets unioned.
  A pack cannot restrict a previously declared value set.

Conflict conditions that are hard errors:

- A declarative pack's `edge_endpoints` entry references a `relation` name that is not one of the
  13 base relations from ADR-002. Declarative packs may only extend which kind-pairs participate
  in existing relations — they cannot introduce new relation names.

Non-error situations:

- A declarative pack entity kind or note kind collides with a Rust built-in kind. Not an error —
  the kind exists in both; vocabulary is idempotent.
- Two declarative packs declare the same entity kind with the same string. Not an error.
- A declarative pack declares a property key that an existing pack already declared with the same
  `values` set. Not an error.

After merging, the runtime writes the resolved vocabulary into the `entity_kinds`, `note_kinds`,
and `edge_relations` sections of `schema.yaml`. This write is performed by `khive pack install`
and `khive pack remove` — not on every startup. The `schema.yaml` serves as a cache of the last
validated merged state.

### 4. Local CLI commands

All `khive pack` commands are Deno CLI commands living in `deno/src/pack/`. They follow the same
conventions as `khive kg` commands (ADR-048).

#### Authoring

```
khive pack init
```

Creates a `pack.yaml` template in the current directory with all required fields and commented
examples. The generated template includes the full list of base entity kinds (from ADR-001) and
base edge relations (from ADR-002) as comments to guide the author.

#### Validation

```
khive pack check <path-to-pack.yaml>
```

Validates a pack manifest without installing it. Checks:

1. Name and version format constraints
2. Kind name format constraints
3. `edge_endpoints` entries reference only existing base relations from ADR-002 (no new relation
   names permitted)
4. Property key format constraints
5. No `values` list contains duplicate strings

Exits with code 0 and prints a summary on success. Exits with non-zero and a structured error
report on failure. This command has no side effects — it does not modify `schema.yaml` or the
local pack cache.

```
khive pack validate
```

Validates all packs currently declared in `.khive/kg/schema.yaml` can be resolved and their
combined vocabulary is conflict-free:

1. Every `builtin` pack is present in the loaded binary's registry.
2. Every `registry` pack exists in the local cache at the declared version.
3. Every `local` pack path resolves to a valid `pack.yaml`.
4. Every `git` pack commit SHA resolves (same remote resolution logic as ADR-048 cross-repo refs).
5. The merged vocabulary has no hard-error conflicts.
6. The `entity_kinds`, `note_kinds`, and `edge_relations` sections in `schema.yaml` match the
   computed merged vocabulary (drift detection).

Exits with code 0 on a clean state. Exits non-zero with a structured report on any violation.

#### Installation

```
khive pack install <name>
khive pack install <name>@<version>
khive pack install ./path/to/pack-directory
khive pack install <github-owner>/<repo>@<ref>
```

Resolves the pack, writes the pack entry to `schema.yaml#packs`, and re-computes the merged
vocabulary sections. Steps:

1. Fetch `pack.yaml` from the source (registry, local path, or git).
2. Run `check` validation on the fetched manifest.
3. Detect vocabulary conflicts with the existing `schema.yaml` vocabulary.
4. If conflict-free, write the entry to `schema.yaml#packs` and update `entity_kinds`,
   `note_kinds`, and `edge_relations`.
5. Cache the fetched `pack.yaml` at `~/.khive/packs/<name>/<version>/pack.yaml` for
   `source: registry` and `source: git`.

For `source: local`, no caching is performed — the path is read on every `validate` and `import`
call.

`khive pack install` is idempotent: installing an already-installed pack at the same version is a
no-op. Installing a new version replaces the existing entry.

```
khive pack remove <name>
```

Pack removal follows the atomic `remove_pack` sequence defined in ADR-054: (1) check for
entities using pack-owned kinds; (2) if found, require `--migrate-to <kind>` or refuse;
(3) execute data migration to re-kind affected entities; (4) remove the pack entry from
`schema.yaml#packs`; (5) recompute the merged vocabulary sections (`entity_kinds`,
`note_kinds`, edge endpoint rules); (6) bump `ontology_version` (major increment, because
kind removal is a breaking change). Does not delete the local cache — the cached `pack.yaml`
remains at `~/.khive/packs/<name>/`.

#### Discovery (cloud-dependent stubs in v0.3)

```
khive pack search <query>
khive pack info <name>
```

In P1-P3, these commands print a message directing the user to the cloud registry. Full
implementation requires ADR-033. The commands are defined in v0.3 with stub behavior so that
help text and command routing work before the registry exists.

#### Publishing (cloud-dependent, v0.5)

```
khive pack publish
```

Publishes the `pack.yaml` in the current directory to the cloud registry. Requires authentication
against the cloud registry (ADR-033). Full implementation deferred to P5.

### 5. Coexistence with compiled Rust packs

Rust built-in packs (`khive-pack-kg`, `khive-pack-gtd`, `khive-pack-memory`) register their
vocabulary and verb handlers at binary startup via the `Pack` and `PackRuntime` traits. This
mechanism is unchanged. Declarative YAML packs **do not replace** Rust packs — they complement
them.

The coexistence model:

- Rust packs own **verb handlers**. Declarative packs are vocabulary-only; they cannot register
  new verb handlers in v0.3. An `ml-papers` pack can declare `model` as an entity kind, but the
  `create`, `search`, and `list` verbs that operate on `model` entities are still served by the
  `kg` pack's handlers (which accept any kind string registered in the merged vocabulary).
- If a declarative pack declares the same entity kind as a Rust built-in pack, the merged
  vocabulary sees one kind (idempotent union). There is no conflict.
- The runtime resolves vocabulary first from the merged set (Rust + YAML), then dispatches verbs
  to the Rust pack handlers that own them. Declarative packs add to the vocabulary that those
  handlers validate against.

This design means declarative packs provide vocabulary coverage for all existing verb handlers
without requiring new code. A researcher can install `ml-papers` and immediately use
`create(kind="model", ...)` — the `kg` pack's `create` handler already accepts any entity kind
in the merged vocabulary.

### 6. Pack storage locations

```
~/.khive/packs/                              # global pack cache (per-user)
  ml-papers/
    1.0.0/
      pack.yaml
    1.1.0/
      pack.yaml
  software-arch/
    0.3.2/
      pack.yaml

<project-root>/
  .khive/
    kg/
      schema.yaml                            # declares installed packs + merged vocabulary
      entities.ndjson
      edges.ndjson
    settings.json                            # actor config (namespace, etc.)
```

The global pack cache (`~/.khive/packs/`) stores registry and git packs. Local packs are not
cached — they are read from their declared path. The cache is shared across all projects on the
machine; a pack version installed in one project is available to all projects without re-fetching.

The cache directory should be excluded from version control. `khive pack init` appends
`~/.khive/packs/` to `.gitignore` if the project has a `.gitignore`.

### 7. CI integration

`khive pack validate` integrates into the CI workflow generated by `khive kg init` (ADR-048 §6).
The `kg-validate.yml` GitHub Actions workflow gains a pack validation step:

```yaml
- run: khive pack validate
```

This step runs after `khive kg validate` and ensures that:

- All declared packs resolve at the pinned versions/SHAs.
- The merged vocabulary in `schema.yaml` matches the computed merged vocabulary (no drift).
- No vocabulary conflicts have been introduced.

The workflow fails if any pack cannot be resolved or if vocabulary conflicts exist.

### 8. Phasing

| Phase | What                                                                                          | Target version |
| ----- | --------------------------------------------------------------------------------------------- | -------------- |
| P1    | `pack.yaml` manifest format specification + `khive pack check` validator                      | v0.3           |
| P2    | `schema.yaml#packs` section + vocabulary merging in `khive pack validate`                     | v0.3           |
| P3    | `khive pack init` + `khive pack install` (local path only, no registry)                       | v0.4           |
| P4    | Pack cache + `khive pack install` from registry and git (requires cloud ADR-033 for registry) | v0.5           |
| P5    | `khive pack publish` (requires cloud ADR-033 authentication)                                  | v0.5           |

P1 and P2 are independently shippable and establish the format contract. Third-party tools can
validate and author `pack.yaml` files before `khive pack install` exists. P3 enables the local
development workflow. P4 and P5 are cloud-dependent.

## Rationale

### Why YAML rather than TOML or JSON

YAML is already used for `schema.yaml` (ADR-048), establishing a project-wide convention.
Consistency within the `.khive/` directory reduces cognitive load for authors switching between
files. YAML also reads more naturally than TOML for the hierarchical `edge_endpoints` structure
(nested `endpoints` arrays with source/target pairs). JSON is too verbose for a hand-authored
manifest.

### Why declarative packs are vocabulary-only, not code

Allowing declarative packs to contain arbitrary code (scripts, WASM modules) would create a
security surface. A pack downloaded from the registry could execute arbitrary code on the user's
machine during `khive pack install`. Keeping packs as pure vocabulary declarations means the
worst a malicious pack can do is pollute the vocabulary — a recoverable, auditable change that
`khive pack remove` reverses. Code extension remains gated behind the Rust pack mechanism, which
requires compilation and intentional binary distribution.

### Why edge endpoint extension rather than new relation names

The 13-relation closed enum (ADR-002) is the semantic backbone of khive. Its stability allows
traversal, query compilation, and tooling to reason about graph structure without inspecting
pack configuration. Opening the enum to arbitrary new relation names would fragment the traversal
semantics — two packs might declare `trained_on` and `fine-tuned-from` as separate relations
when both express a `depends_on` that could be unified.

The correct extension point is the endpoint contract: a pack that adds `model` and `dataset` kinds
declares that `depends_on(model, dataset)` is a valid triple. The traversal engine already knows
how to follow `depends_on` edges — it does not need to learn anything new. The pack contributes
domain meaning (what it means for a model to depend on a dataset) without fragmenting the relation
namespace. This is consistent with the extension mechanism in ADR-031.

### Why write the merged vocabulary back to `schema.yaml`

The ADR-048 design principle is that `schema.yaml` is self-contained for validation even without
fetching external resources. Writing the merged `entity_kinds`, `note_kinds`, and `edge_relations`
after pack operations preserves this property for vocabulary: an agent validating the KG with
`khive kg validate` sees the full vocabulary without needing to resolve all packs. The `packs`
section serves as the authoritative source of truth; the merged vocabulary sections serve as a
derived but committed cache.

### Why the global pack cache is per-user, not per-project

Registry and git packs are version-pinned. The same `ml-papers@1.0.0` across ten projects is
identical content. Caching per-user avoids fetching the same pack multiple times and allows
offline work after the first fetch. The per-project `schema.yaml` records which version is in
use, making the project state reproducible even if the global cache is cleared and repopulated.

### Why `khive pack search` is a stub in P1-P3

Implementing pack search requires the cloud registry (ADR-033) to exist. Stubbing the command
now allows help text, shell completion, and command routing to work before the registry is
available. This avoids the "command not found" confusion when the registry ships and users
discover it via documentation that references `khive pack search`. Stubs print a clear message
pointing to the cloud registry documentation.

## Alternatives Considered

| Alternative                                                | Pros                                                        | Cons                                                                                                                                                         | Why rejected                                                                                                  |
| ---------------------------------------------------------- | ----------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------- |
| TOML manifest (pack.toml)                                  | Familiar to Rust authors; no ambiguous YAML indentation     | Inconsistent with existing schema.yaml convention; TOML's nested array syntax is more verbose for endpoint pairs                                             | Convention consistency with ADR-048 wins                                                                      |
| Allow packs to contain WASM verb handlers                  | Full extensibility without recompilation                    | Security surface; WASM sandboxing complexity; binary distribution of user-compiled modules                                                                   | Security model does not support arbitrary code in downloaded packs                                            |
| Allow packs to declare new relation names (as strings)     | Domain-specific edge semantics without ADR amendment        | Fragments traversal semantics; two packs may express the same semantic with different names; tooling cannot reason generically about pack-specific relations | The 13-relation closed set is the traversal backbone; packs extend endpoint pairs, not the relation namespace |
| Store merged vocabulary only in memory, not in schema.yaml | Avoids schema.yaml churn during pack changes                | schema.yaml loses self-containment for offline validation; CI must always fetch all packs                                                                    | ADR-048 self-containment principle is worth the churn cost                                                    |
| Per-project pack cache (inside `.khive/`)                  | Fully isolated per project; committed alongside the project | Bloats the git repo with downloaded manifests; version-identical packs duplicated across projects                                                            | Pack manifests are small but the principle of committing derived artifacts is wrong                           |
| Registry-first design (no local install)                   | Simpler architecture; one install path                      | Blocks offline development and monorepo workflows; cloud dependency for local validation                                                                     | OSS must be usable without cloud infrastructure                                                               |

## Consequences

### Positive

- Third-party vocabulary extension without Rust development. A researcher with domain knowledge
  but no Rust experience can author a `pack.yaml` and install it locally with `khive pack install
  ./path/to/pack`.
- The pack format is language-neutral and machine-readable. Tools that build on khive (IDE
  plugins, notebook integrations, alternative frontends) can read and validate `pack.yaml` files
  without running the Rust binary.
- The cloud marketplace (ADR-033) has a stable, versioned format to build on. Publishing a pack
  is submitting a `pack.yaml` — the format is fully specified before the registry exists.
- `schema.yaml` remains self-contained for validation, preserving the ADR-048 property that
  offline environments can validate a KG without fetching external resources.
- Rust built-in packs are unaffected. Existing users and agent integrations see no changes.

### Negative

- Declarative packs cannot register verb handlers. A domain that needs custom verb logic (not
  just vocabulary) still requires a Rust pack. The boundary between "vocabulary extension" and
  "behavior extension" may frustrate some users.
- Declarative packs cannot introduce new relation names. Domain authors who need a semantic
  distinction not covered by the 13 base relations must file an ADR-002 amendment rather than
  solving it in a pack.
- Writing merged vocabulary back to `schema.yaml` means pack operations produce a git-visible
  diff. Teams must understand that `git diff .khive/kg/schema.yaml` after `khive pack install`
  is expected and should be committed.
- CI must resolve all declared packs (or use the local cache) to validate the full vocabulary.
  An environment without network access and without a warm cache will fail `khive pack validate`.

### Neutral

- Rust built-in pack vocabulary and declarative pack vocabulary merge idempotently. The
  `entity_kinds` list in `schema.yaml` includes both, and `khive kg validate` does not
  distinguish between their origins.
- Pack cache invalidation is simple: the cache key is `(name, version)` for registry packs and
  `(repo, sha)` for git packs. Cache entries are immutable once written. No expiration logic.
- The ADR-048 `schema.yaml` format version (`1.0.0`) must be incremented to `1.1.0` when the
  `packs` section is added, because `packs` is an optional key: consumers that validate against
  v1.0.0 JSON Schema will reject a v1.1.0 file that has the new key. The minor bump is correct
  under ADR-048's version policy (new optional key = minor increment).

## Implementation

### Deno CLI structure

New commands live in `deno/src/pack/`:

```
deno/src/pack/
  mod.ts          -- pack subcommand routing
  check.ts        -- khive pack check <path>
  validate.ts     -- khive pack validate
  init.ts         -- khive pack init
  install.ts      -- khive pack install <...>
  remove.ts       -- khive pack remove <name>
  search.ts       -- khive pack search <query> (stub)
  info.ts         -- khive pack info <name> (stub)
  publish.ts      -- khive pack publish (stub)
  schema.ts       -- PackManifest type + YAML parser + format validator
  merge.ts        -- vocabulary merge logic (declarative + builtin)
  cache.ts        -- ~/.khive/packs/ read/write
```

`schema.ts` is the canonical TypeScript definition of the `pack.yaml` format. It exports a
`PackManifest` interface matching the YAML structure defined in §1 and a `validatePackManifest`
function that returns a structured error report.

### schema.yaml changes

`schema.yaml` gains the `packs` top-level key. The schema format version bumps from `1.0.0` to
`1.1.0`. The embedded JSON Schema in `crates/khive-vcs/src/schema/` gains `v1.1.json` with the
`packs` array definition.

Existing `schema.yaml` files without a `packs` key remain valid (the key is optional; its
absence is treated as `packs: []` — vocabulary is entirely from Rust built-in packs).

### Runtime changes (Rust)

The Rust runtime in `crates/khive-runtime/` gains a `DeclarativePack` struct that implements
`PackRuntime` by loading a resolved `pack.yaml` at startup. The transport layer
(`crates/khive-mcp/`) reads `schema.yaml` at startup and registers one `DeclarativePack` per
entry in `packs` whose `source` is not `builtin`.

```rust
// crates/khive-runtime/src/declarative_pack.rs
pub struct DeclarativePack {
    manifest: PackManifest,   // deserialized pack.yaml
}

impl PackRuntime for DeclarativePack {
    fn name(&self) -> &str { &self.manifest.name }
    fn entity_kinds(&self) -> &[String] { &self.manifest.entity_kinds }
    fn note_kinds(&self) -> &[String] { &self.manifest.note_kinds }
    // edge_rules(): converts manifest edge_endpoints to EdgeEndpointRule
    // dispatch(): returns Err(RuntimeError::UnknownVerb) for all verbs
    //             (declarative packs own vocabulary, not verb handlers)
}
```

`PackManifest` in Rust is a `serde`-deserialized counterpart to the TypeScript `PackManifest`
interface, defined in a new `crates/khive-pack-format/` crate (Apache-2.0) so it can be used by
both the Rust runtime and any external Rust tooling without depending on the full runtime.

### JSON Schema for validation

`pack.yaml` validation in `khive pack check` uses a JSON Schema embedded in the Deno CLI.
The schema is committed at `deno/src/pack/pack-schema.json` and validates:

- Required fields: `name`, `version`
- Optional fields: `description`, `author`, `license`, `homepage`, `entity_kinds`, `note_kinds`,
  `edge_endpoints`, `properties`
- Format patterns for `name` and kind strings
- Structural constraints on `edge_endpoints` entries

The same JSON Schema is published at `https://khive.ai/schemas/pack/v1.json` (cloud ADR-033
responsibility) so external editors (VS Code YAML extension, etc.) can validate `pack.yaml`
files with `# yaml-language-server: $schema=https://khive.ai/schemas/pack/v1.json`.

### Phasing detail

| Phase | Deliverables                                                                                                                                                                   |
| ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| P1    | `deno/src/pack/schema.ts` (PackManifest type + validator), `khive pack check`, `pack-schema.json`, `crates/khive-pack-format/` crate (PackManifest struct + serde)             |
| P2    | `schema.yaml` v1.1.0 format with `packs` section, `khive pack validate`, `deno/src/pack/merge.ts`, `crates/khive-vcs/src/schema/v1.1.json`, vocabulary drift detection         |
| P3    | `khive pack init`, `khive pack install ./local-path`, `deno/src/pack/install.ts` (local source only), `deno/src/pack/remove.ts`, `schema.yaml` write-back after install/remove |
| P4    | `deno/src/pack/cache.ts` (`~/.khive/packs/` read/write), `khive pack install <name>@<ver>` (registry source, requires ADR-033 registry API), git source install                |
| P5    | `khive pack publish`, registry authentication (ADR-033)                                                                                                                        |

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md): Entity Kind Taxonomy (base entity kinds that
  declarative packs extend)
- [ADR-002](ADR-002-edge-ontology.md): Closed Edge Ontology (base edge relations; declarative
  packs add endpoint pairs, not new members of the base set)
- [ADR-019](ADR-019-note-kind-taxonomy.md): Note Kind Taxonomy (base note kinds)
- [ADR-025](ADR-025-pack-standard.md): Pack Standard (the Rust pack mechanism; declarative packs
  complement it for vocabulary-only use cases)
- [ADR-031](ADR-031-pack-extensible-edge-endpoints.md): Pack-Extensible Edge Endpoints (the
  additive endpoint contract that declarative `edge_endpoints` entries follow)
- [ADR-037](ADR-037-inter-pack-vocabulary-dependencies.md): Inter-Pack Vocabulary Dependencies
  (load-order and dependency declarations; declarative packs currently have no `REQUIRES`
  mechanism — a future extension)
- [ADR-048](ADR-048-git-native-kg-versioning.md): Git-Native KG Versioning (`schema.yaml` format
  this ADR extends; SHA-pin discipline for `source: git` packs)
- Cloud ADR-033: Pack Marketplace (registry, publishing, social features built on this OSS
  foundation)
- YAML specification: https://yaml.org/spec/1.2.2/
- semver specification: https://semver.org/
