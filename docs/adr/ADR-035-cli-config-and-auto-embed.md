# ADR-035: CLI Configuration and Automatic Embedding

**Status**: accepted (amended 2026-08-01)
**Date**: 2026-05-23
**Authors**: khive maintainers

## Context

ADR-020 defines the `.khive/` directory layout, the NDJSON format, and the `kkernel kg commit`
and `kkernel kg sync` pipelines. ADR-028 defines pack-scoped backend assignment via
`khive.toml`. Neither ADR addresses two practical concerns that affect every project using
the git-native KG workflow:

1. **Runtime configuration**: the embedding model, device preferences, and schema strictness
   are hard-coded defaults. Different projects may need different models; different machines
   have different inference hardware. There is no way to record these settings alongside the
   KG data or override them at the user level without recompiling.

2. **Automatic embeddings**: `kkernel search` uses hybrid FTS + vector search (ADR-012).
   Vectors must exist in `working.db` for the vector component to contribute. Without
   automatic embedding on commit and sync, vectors grow stale and search quality degrades
   silently — no error, just worse results.

### One config schema and one selected file

ADR-028 introduces the khive TOML schema for deployment topology:
`[[backends]]`, `[[engines]]`, and `[packs.*]` sections. This ADR adds embed and
schema settings to that same schema; it does not create a second sidecar file
with an overlapping purpose.

The accepted loader discovers these filenames, in precedence order:

| File                   | Scope                               | Committed                         |
| ---------------------- | ----------------------------------- | --------------------------------- |
| `./khive.toml`         | Project-root compatibility location | Operator choice                   |
| `./.khive/config.toml` | Canonical project-local location    | Yes — shared across collaborators |
| `~/.khive/config.toml` | User-global fallback                | No — machine-specific             |

Only the first existing file is loaded. The files are not merged per key.
`.khive/khive.toml` is not a discovery tier and must never be silently treated
as one. ADR-020's `.khive/.gitignore` allowlist includes `config.toml`.

### The consistency requirement

Embedding vectors are only comparable if produced by the same model. If Alice commits with
`all-minilm-l6-v2` (384 dimensions) and Bob syncs with `BGE-small` (same dimension count but a
different model), their vectors are numerically incompatible: cosine similarity across models
is meaningless. The project-level `.khive/config.toml` (or accepted root
`khive.toml` alternative) must specify the embedding model, and that file must
be committed so all collaborators use the same model.

This means:

- The embedding model is a **project-level setting** — committed to git, enforced across
  the team, not overridable per-user.
- Device preferences are a provider concern, not a second model-selection authority.
  Under the 2026-10-10 amendment, legacy `embed.device` is recognized but ignored; this ADR
  introduces no device selector or per-key merge of a second TOML file.

## Decision

### 1. Unified TOML schema

The selected config file carries all configuration for khive. ADR-028's
`[[backends]]`, `[[engines]]`, and `[packs.*]` sections are joined by
`[embed]` and `[schema]` sections from this ADR.

**Canonical project-level file** (`.khive/config.toml` — committed to git):

The `[embed]` and `[schema]` settings in this expanded example express the 2026-10-10
amendment's contract, which is accepted but not yet implemented. They are not a claim of
shipped support. The minimal initializer template in §4 remains engine-only.

```toml
# .khive/config.toml — project configuration
# Committed to git. All collaborators use these settings.
# See: ADR-028 (backends/packs) and ADR-035 (embed/schema).

# --- Backend and pack topology (ADR-028) ---

[[backends]]
name = "main"
kind = "sqlite"
path = "~/.khive/khive.db"

[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
dims = 384

[packs.kg]
backend = "main"

[packs.memory]
backend = "main"

[packs.gtd]
backend = "main"

# --- Embedding pipeline configuration (2026-10-10 amendment, not yet implemented) ---

[embed]
auto_embed = true           # automatic serving work; explicit reindex still runs
batch_size = 128            # reindex batch size; CLI --batch-size takes precedence

[embed.fields]
include = ["name", "description"]  # entity fields concatenated for embedding

# --- Import validation (2026-10-10 amendment, not yet implemented) ---

[schema]
strict = true               # false admits unknown entity kinds; relations stay closed
```

**User-global fallback** (`~/.khive/config.toml` — not committed):

```toml
# ~/.khive/config.toml — user defaults, not committed to any project

[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
```

Only keys that diverge from built-in defaults need to appear. The global file
is selected only when neither project location exists; it is not merged into a
selected project file.

### 2. Configuration resolution order

Configuration-file discovery is **explicit `--config` / `KHIVE_CONFIG` path

> project-root `./khive.toml` > DB-anchored or cwd project
> `./.khive/config.toml` > `~/.khive/config.toml` > no file**. The first file
> that exists is parsed and validated; a malformed higher-precedence file is an
> error, not a reason to continue to a lower tier.

When an explicit database path is supplied, the hidden project tier is
anchored beside that resolved database so a thin client and its daemon select
the same file. With no explicit database path, it is anchored to the current
project directory. This is the ADR-096 `config_id` coherence rule.

`kkernel mcp`, `kkernel exec` (including `--pending-events`), and `kkernel
reindex` all expose the explicit `--config` / `KHIVE_CONFIG` tier and thread it
through every post-resolution config reload. An entry point must not document
this tier while silently falling back to automatic discovery.

There is no per-key merge between project and global files. A machine-local
setting that must coexist with committed project settings uses the applicable
CLI or environment override.

## CLI / env / config precedence

For each runtime option, precedence is:
**CLI flag > selected config file > applicable `KHIVE_*` env var > built-in
default**. Exact option-specific exceptions are listed in the canonical config
reference (`docs/khive-config-example.toml`).

Pack selection is the exception specified by ADR-027 Amendment 3: `--pack` >
`KHIVE_PACKS` > `runtime.packs` > the built-in production set. Each layer replaces
the complete set; an empty layer falls through rather than selecting zero packs.

| Option                              | CLI flag                         | Env var                             | Config key                | Default           |
| ----------------------------------- | -------------------------------- | ----------------------------------- | ------------------------- | ----------------- |
| Namespace                           | `--namespace`                    | `KHIVE_NAMESPACE`                   | `runtime.namespace`       | `default`         |
| Loaded packs                        | `--pack` (repeat)                | `KHIVE_PACKS`                       | `runtime.packs`           | production set    |
| DB path                             | `--db`                           | `KHIVE_DB`                          | `runtime.db_path`         | `~/.khive/kg.db`  |
| Recall min_score                    | (n/a, per-call)                  | `KHIVE_RECALL_MIN_SCORE`            | `memory.recall.min_score` | `None` (no floor) |
| Disable embeddings                  | `kkernel mcp --no-embed`         | `KHIVE_NO_EMBED`                    | (none)                    | `false`           |
| Automatic embedding (unimplemented) | `--no-embed` forces off          | `KHIVE_NO_EMBED=1` forces off       | `embed.auto_embed`        | `true`            |
| Reindex batch size (unimplemented)  | `kkernel reindex --batch-size`   | (none)                              | `embed.batch_size`        | `128`             |
| Reindex model                       | `kkernel reindex --model <name>` | `KHIVE_EMBEDDING_MODEL`             | `[[engines]]`             | built-in engine   |
| Additional models                   | (none)                           | `KHIVE_ADDITIONAL_EMBEDDING_MODELS` | `[[engines]]`             | none              |
| Log level                           | `--log-level`                    | `KHIVE_LOG`                         | `runtime.log_level`       | `info`            |
| Authorization gate                  | `--gate`                         | `KHIVE_GATE`                        | `runtime.gate`            | `allow-all`       |
| Brain profile                       | `--brain-profile`                | `KHIVE_BRAIN_PROFILE`               | `runtime.brain_profile`   | `None`            |

The shipped disable row stays as accepted. The automatic-embedding row from the 2026-10-10
amendment adds its config key beside it and is a force-off exception to the general precedence:
either existing no-embed control overrides `auto_embed=true`; no force-on flag or environment
variable is added. Reindex batch precedence is explicit CLI value > selected config > 128,
with no environment tier. Explicit reindex ignores only `auto_embed`, as specified below.

Note: `recall(min_score)` has **no floor by default**. Operators serving larger corpora should
set `KHIVE_RECALL_MIN_SCORE=0.5` (or similar) in production deployments.

### Brain profile configuration

The `brain_profile` option designates which brain profile receives feedback from
`memory.feedback` and `knowledge.feedback`, and from which profile recall-time score
boosting reads. It is configured the same way namespace is — via `--brain-profile`,
`KHIVE_BRAIN_PROFILE`, or `runtime.brain_profile` in `khive.toml`.

**Configuration example** (`.khive/config.toml`):

```toml
[runtime]
namespace = "local"
brain_profile = "project-recall-v1"
```

**Feedback and recall-boost profile resolution order** (for `memory.feedback`,
`knowledge.feedback`, and recall-time boosting):

1. **Explicit profile in config**: if `runtime.brain_profile` / `KHIVE_BRAIN_PROFILE` /
   `--brain-profile` resolves to a non-empty string, that profile ID is used directly.
2. **Actor/namespace-bound profile**: when no explicit profile is set, resolve
   against the actual caller and namespace. Memory/recall use `consumer_kind="recall"`;
   knowledge uses `consumer_kind="knowledge_compose"` and accepts only a matched binding.
3. **Pack-local tuning prior**: if neither explicit nor namespace-bound profile resolves, the
   pack-local in-memory state receives the update directly. `BalancedRecallState` retains the
   original memory-pack fallback. As amended by #1505, knowledge's `SectionPosteriorState` is
   keyed by the effective namespace so an explicit measurement arm cannot inherit live/local
   compose feedback. The default `local` path remains backward compatible.

This resolution is automatic: packs attempt tiers 1 and 2 silently and fall through to tier 3
when nothing is bound. No configuration is required for the default-namespace fallback to
continue working as before.

### 3. `[embed]` and `[schema]` sections

**2026-10-10 amendment contract — accepted, pending implementation.**
`[embed]` controls the automatic pipeline; `[schema]` controls import validation. Neither
registers engines. The existing `--no-embed` / `KHIVE_NO_EMBED` controls and explicit
`kkernel reindex` workflow remain; no `--auto-embed` or `KHIVE_AUTO_EMBED` control is added.

**Defaults** (when the selected file omits the setting):

| Key                    | Default                   |
| ---------------------- | ------------------------- |
| `embed.auto_embed`     | `true`                    |
| `embed.batch_size`     | `128`                     |
| `embed.fields.include` | `["name", "description"]` |
| `schema.strict`        | `true`                    |

`embed.model`, `embed.dimensions`, and `embed.device` are recognized legacy keys with no
selection effect. They have no active defaults: `[[engines]]` alone determines the model
registry. Presence does not itself refuse a config, but invalid known values still fail §8
validation. Compatibility warnings are specified in §9.

`auto_embed=false` has the same serving effect as `--no-embed` at MCP, exec forwarding and
daemon entry points: it disables built-in automatic embedding, including startup backfill
and query-vector fallback. Custom provider registration retains its existing no-embed
behavior. `--no-embed` or `KHIVE_NO_EMBED=1` forces off regardless of config. The resolved
automatic policy and ordered field selection enter the forwarding identity, so a client
cannot silently reuse a daemon with a different effective policy.

`batch_size` applies only to `kkernel reindex`, including its existing knowledge pass; it
does not set global import/backfill concurrency. An explicit CLI `--batch-size` takes
precedence over config, which takes precedence over 128. The 128 default replaces the
historical design value 64 to preserve shipped no-config behavior. An explicitly stored 64
remains 64; no config is rewritten. Config values must be positive integers. The effective
maximum is 500, and the CLI's existing zero-to-one clamp remains. Explicit flag presence is
retained: `--id` plus an explicitly supplied `--batch-size` is refused even when the value
equals the default or resolved batch.

`embed.fields.include` is an ordered list for **entity vector input only**. `name` and
`description` select the top-level entity fields. Every other selector is a literal key in
`entity.properties`; `a.b` is not nested traversal. `kind` is forbidden. Empty arrays, blank
selectors and duplicate selectors fail config load. Valid literal keys are not trimmed or
normalized into another key.

Selected string values contribute their exact bytes; absent, null and non-string values
contribute nothing. Contributed fields are joined with one ASCII space, without trimming
stored text, stringifying JSON values or automatically adding field names. The default
`["name", "description"]` retains the existing constructor exactly: when description is
nonempty, use `name + " " + description`; otherwise use `name`. An empty description adds
no separator; an empty name with a nonempty description retains the leading space.

Whitespace-only selected input makes no embedding request and inserts no vector. FTS still
updates normally. Ordinary update/repair preserves an existing vector when no new input is
embedded; no vector deletion or new compare-and-set contract is introduced. Changes to
`include` do not rebuild existing vectors automatically; explicit reindex applies the new
selection subject to its existing preservation rules (§5). Notes and their prefixes,
knowledge content, FTS document shape/ranking, tags and note-kind policy are unchanged.

For explicit `kkernel kg import` and `KhiveRuntime::import_kg` calls, `schema.strict=true`,
the default, refuses unknown entity kinds and unknown edge relations before import writes.
With `strict=false`, those calls may retain unknown
entity kinds with their spelling as given and emits one warning per distinct unknown kind.
Existing alias handling and normalization for known or pack-registered kinds remain
unchanged. Unknown relations are refused in **both** modes: the ADR-002 relation vocabulary
stays closed. Relaxed import does not register a new kind, relax ordinary create validation,
or bypass other validation, including endpoint rules, reserved properties and secret checks.
This preflight guarantee does not introduce transactionality for later operational failures.
The entry-point boundary and its scoped relationship to ADR-001's VCS snapshot
rule are specified in the amendment's [import policy boundary](#import-policy-boundary).

### 4. `kkernel kg init` writes `.khive/config.toml`

`kkernel kg init` writes a minimal, valid `.khive/config.toml` that pins the
default embedding engine in the schema the accepted loader consumes:

```toml
# .khive/config.toml — project KG configuration
# Committed to git. All collaborators use these settings.

[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
dims = 384
```

If `.khive/config.toml` already exists, `init` does not overwrite it. The
non-overwrite guarantee uses an atomic create rather than an existence check
followed by a truncating write. If root `khive.toml` already exists, init
preserves that accepted higher-precedence config and does not create a hidden
file that the loader would ignore.

`.khive/khive.toml` is the obsolete initializer spelling and is not a loader
tier. When it exists, init fails before writing scaffolding and names both the
legacy and canonical paths. When legacy and canonical files both exist, init
fails without modifying either one; the operator must reconcile them
explicitly.

The `.khive/.gitignore` allowlist from ADR-020 adds `config.toml` alongside
`kg/`:

```gitignore
*
!.gitignore
!kg/
!kg/**
!config.toml
```

Init automatically updates only the byte-exact `.gitignore` emitted by the
old initializer (`!khive.toml` to `!config.toml`). It never rewrites a
customized ignore file.

### 5. Shipped embedding and repair workflow

The original decision specified an `embed_missing` pass in `kkernel kg commit` and
`kkernel kg sync`, plus a `kkernel kg embed` command. That automatic-embedding behavior and
dedicated command are not present in the current Rust CLI. `kkernel kg commit` validates and
commits a staged change-set; the current `kkernel kg sync` spelling is a visible alias for remote
fetch; and top-level `kkernel sync` rebuilds the SQLite database and FTS documents from NDJSON.
None invokes an embedding pass, and `kkernel kg` has no `embed` subcommand.

The shipped behavior has two parts:

1. Runtime create and update paths embed entities and ordinary notes inline for every
   configured engine. Pack-declared note-kind policies may narrow the set: the comm
   pack's `message` notes use only the default engine unless a caller explicitly
   selects another model.
   `kkernel mcp --no-embed` (or `KHIVE_NO_EMBED=1`) starts the MCP runtime without any
   built-in embedding engine, so those writes remain text-only.
2. `kkernel reindex` is the explicit maintenance and repair command. It rebuilds vectors and
   FTS documents for entities, notes, and, by default, the knowledge corpus. It resolves the
   same database, config, namespace, and `[[engines]]` set as `kkernel mcp`.

Examples using only shipped flags:

```bash
# Re-embed entities with every configured engine and notes per kind policy;
# knowledge uses the default engine.
kkernel reindex --db ~/.khive/khive.db --namespace local

# Repair only the graph substrate and keep vectors that already exist.
kkernel reindex --db ~/.khive/khive.db --namespace local \
  --no-knowledge --keep-existing

# Rebuild entity/note vectors with one named engine, overriding note-kind policy.
kkernel reindex --db ~/.khive/khive.db --namespace local \
  --no-knowledge --model all-minilm-l6-v2
```

Without `--keep-existing`, every eligible staged record is re-embedded and each prior vector is
replaced atomically with its new value; a failed embed or insert leaves the prior vector in
place rather than deleting it first. With `--keep-existing`, records already embedded for
the selected model and namespace are skipped. FTS backfill still runs in either mode. The
default is fail-closed on partial failures; `--best-effort` is the explicit opt-in to a zero
exit after partial work.

There is no current `--embeds-only`, `--ids`, or `--dry-run` reindex mode. In particular,
`--keep-existing` means incremental vector top-up, not vector-only execution. When no
embedding engine is configured, `kkernel reindex` still backfills FTS but warns and skips
vector work. An operator who normally runs the server with `--no-embed` can therefore run a
separate `kkernel reindex` invocation without that server flag, using a config that declares
the desired `[[engines]]`, to populate vectors on an explicit schedule.

**Configuration interaction (2026-10-10 amendment):** explicit `kkernel reindex`, including
`--id` repair, ignores only `embed.auto_embed`. It uses the same selected file, engines,
ordered entity fields and namespace rules; it neither reloads a different config nor reconstructs a default
engine. Note-kind restrictions remain unless an explicit `--model` overrides them. Existing
failed-embedding/failed-insert retention, `--keep-existing` skips, healthy-vector preservation
under `--id`, namespace/revision fences and partial-failure reporting remain in force. A full
reindex does not replace a vector with an embedding of whitespace-only input, and does not
promise to purge that old vector. FTS and knowledge input rules remain unchanged.

### 6. Embeddings are local-only derived state

Vectors are stored in `working.db` only. They are **not** written to NDJSON files and are
**not** committed to git. Three reasons:

- **Recomputable**: vectors are a deterministic function of the entity text and the
  embedding model. They carry no information beyond what `khive.toml` (model) and
  `entities.ndjson` (text) already record.
- **Size**: 384 floats per entity is 1.5 KB. A 10,000-entity KG would add 15 MB of
  non-human-readable binary content to NDJSON, destroying the git diff and merge
  guarantees that are the entire value of ADR-020.
- **Consistency**: `kkernel reindex` recomputes vectors from the selected engine set and
  current entity/note text. There is no durability requirement for the vectors themselves.

`working.db` is gitignored by ADR-020's allowlist. The `.khive/state/` directory is
ephemeral by design.

### 7. Model change workflow

When the project's embedding engine set changes, the existing vectors no longer describe the
selected configuration. The shipped workflow is:

```bash
# 1. Edit the selected config's [[engines]] entries.

# 2. Re-embed graph and knowledge state with that config.
kkernel reindex --config .khive/config.toml --db ~/.khive/khive.db \
  --namespace local

# 3. Commit only the config change; vectors remain local derived state.
git add .khive/config.toml
git commit -m "config: switch embedding engine"
```

After the commit, other collaborators run:

```bash
git pull
kkernel reindex --config .khive/config.toml --db ~/.khive/khive.db \
  --namespace local
```

Under the 2026-10-10 field-selection contract, changing `embed.fields.include` likewise
requires explicit reindex for existing vectors to use the new input. The edit alone causes no
startup rewrite. `--keep-existing` still skips existing vectors, failed replacements preserve
old vectors, and whitespace-only input does not remove an old vector. Set `auto_embed=false`
to suppress automatic serving work while retaining this explicit maintenance workflow.

### 8. Config validation

The following `[embed]` and `[schema]` validation belongs to the 2026-10-10 amendment,
accepted but not yet implemented; it is not a claim that the current loader implements
these keys:

- `embed.auto_embed` is a boolean.
- `embed.batch_size` is a positive integer.
- `embed.fields.include` is a non-empty array of nonblank, nonduplicated strings. `kind`
  is forbidden; `name` and `description` are top-level fields and every other string is a
  literal property key, with the exact-byte rules in §3.
- Legacy `embed.model` is a nonempty string. It does not select or load a provider; a
  different nonempty model name warns as specified in §9 rather than failing availability
  validation.
- Legacy `embed.dimensions` is a positive integer; it does not resize vectors.
- Legacy `embed.device` is one of `metal`, `cuda`, `cpu`. Any present valid value is ignored
  and warns; it selects no hardware and is not restricted to the global fallback file.
- `schema.strict` is a boolean.
- `[[backends]]` and `[[engines]]` sections are validated per ADR-028.

Unknown keys produce a warning but do not abort. This allows newer config shapes to
exist without breaking older `kkernel` versions. In particular, unknown `[embed]`,
`[embed.fields]` and `[schema]` keys warn and are ignored, without discarding valid known
settings. This does not loosen deliberately closed unrelated config tables. Unknown keys
are distinct from invalid known values, which abort instead of silently taking a default.

A config parse or validation error (malformed TOML, invalid type or invalid known value)
aborts with a structured message identifying the selected file, key and source location:

```
ERROR: .khive/config.toml line 5: expected integer for embed.dimensions, got "384px"
```

An invalid higher-precedence file never falls through to a lower-precedence file. Import
applies the selected `schema.strict` policy before target writes; an absent section or absent
`strict` keeps the strict default. The relaxed policy is explicit per import and does not
alter the runtime's registered kind vocabulary or the strict defaults of existing library
import entry points.

### 9. Relationship between `[embed]` and `[[engines]]`

`[[engines]]` (ADR-028) declares the process-wide registry of loaded embedding models —
the names and dimensions that `EmbedderRegistry::from_config` uses to instantiate models.

`[[engines]]` remains the sole model authority under the 2026-10-10 amendment. Its canonical
ordered peer contract and legacy-input conversion are supplied by the engine-configuration
prerequisite (#5043); this amendment does not recreate a `default = true` selector as a second
canonical contract. Existing engine input, including the minimal §4 template, remains subject
to that prerequisite's compatibility conversion. The resolved default peer supplies default
model policy; inline entities use the configured engine set, and notes use their installed
note-kind policy. Explicit `--model` retains its reindex override.

Legacy `[embed]` values are recognized for existing configs, including files written by an
older initializer. They select no model, dimensions or device. Emit **one startup compatibility
warning naming `[[engines]]`**, aggregating all applicable reasons:

- `embed.model` differs from the effective default engine's name. With no default engine,
  a supplied model cannot match a default; do not select an engine from that legacy value.
- `embed.dimensions` differs from that engine's declared dimension, when one is declared.
  Without a declaration, make no comparison and do not load a model to infer dimensions.
- `embed.device` is present at all, including `cpu`; no provider device selector is added.

Agreement is silent. Resolve the comparison engine before automatic suppression, so
`auto_embed=false` does not erase the reference engine and fabricate a disagreement.
Repeated use of the config during startup does not multiply this compatibility warning.
Invalid known values still abort under §8. Native provider identity/dimension validation
remains the engine contract; the legacy comparison does not replace it.

## Rationale

### Why one selected config, not a merged pair

Two simultaneously active files in the same project create an unnecessary
split. Operators editing topology (`[[backends]]`) need to be in the same
mental context as operators editing embedding settings (`[embed]`). One
selected file reduces cognitive overhead and produces a single committed diff
that shows the full project configuration change.

The sections are orthogonal in structure (`[[backends]]` vs `[embed]`) and serve different
purposes (ADR-028 topology vs this ADR's embed pipeline), so there is no entanglement —
just cohabitation in one well-sectioned file.

### Why project config wins over the global fallback

The embedding engine set is a project invariant. If a global `~/.khive/config.toml` could
override the project's `[[engines]]`, a collaborator with a different default would silently
produce incompatible vectors. The project config must win on embedding-related keys.

Machine-local overrides use only existing option-specific CLI/environment contracts.
The legacy `embed.device` key is ignored, not a new override. The global file
is a fallback for projects without a project config, not a merge source.

### Why inline writes plus an explicit repair command

Missing vectors degrade semantic search without producing a query error. The current runtime
therefore embeds ordinary create/update writes inline when engines are configured, while
`kkernel reindex` provides a deliberate full or incremental repair after bulk import, NDJSON
sync, or an engine change. Keeping reindex separate from git commit/sync also gives operators a
clear fail-closed maintenance command and avoids documenting lifecycle coupling the Rust CLI does
not implement.

### Why NDJSON files never carry vectors

Vectors in NDJSON would break the git-native positioning. A PR that updates an entity
description would also produce a 384-float vector diff that reviewers cannot interpret.
Merge conflicts on vector fields are semantically meaningless. The separation of committed
text (NDJSON) from derived local state (vectors in `working.db`) is the same principle
as separating source files from build artifacts in a standard software project.

## Alternatives Considered

| Alternative                                                 | Pros                         | Cons                                                                  | Why rejected                                                                       |
| ----------------------------------------------------------- | ---------------------------- | --------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Separate active topology and embedding files in one project | Clear file roles             | Two files to manage; split mental context                             | One selected file is simpler and sufficient                                        |
| Per-key project/global TOML merge                           | Machine-local overlays       | Hidden composite config; client/daemon fingerprint drift risk         | First-file selection is deterministic and auditable                                |
| YAML config format                                          | Familiar                     | Ambiguous parsing; indentation errors in practice                     | TOML is unambiguous; already used in Cargo and this project                        |
| JSON config format                                          | Machine-writable             | No comments; annoying to hand-edit; trailing-comma errors             | TOML is better for human-edited files                                              |
| Vectors stored in NDJSON (committed)                        | Single source of truth       | 15 MB+ non-diffable content per 10K entities; breaks merge guarantees | Recomputable state should not be committed                                         |
| Dedicated committed vector file (separate from NDJSON)      | Separates vectors from text  | Same merge problem; grows with entity count                           | Still recomputable; still breaks git diff                                          |
| Manual repair only                                          | Explicit control             | Silent quality degradation when users forget                          | Inline create/update plus explicit reindex covers both paths                       |
| Embed on every embedding-bearing write                      | Fresh vectors for new writes | Adds model latency to those writes                                    | Shipped default; `mcp --no-embed` is the explicit opt-out                          |
| `embed.model` as a second model selector                    | User flexibility             | Conflicting authorities and incompatible vectors across collaborators | `[[engines]]` owns the registry; legacy embed keys are ignored under the amendment |

## Consequences

### Positive (amendment: brain profile knob)

- `memory.feedback` and `knowledge.feedback` can be directed to a specific brain profile
  through the same config path used by namespace — no per-call parameter needed.
- Deployments that bind a namespace to a brain profile via `brain.bind` benefit automatically
  from tier-2 resolution without any `khive.toml` change.
- The pack-local tuning prior (tier 3) continues without configuration. Memory behavior and the
  default knowledge namespace remain unchanged; explicit knowledge namespaces receive isolated
  fallback state instead of sharing local feedback (#1505).

### Positive

- `kkernel reindex` gives operators one verified command for full rebuilds and incremental
  top-up across the configured engine set.
- The embedding model is recorded in `.khive/config.toml`, committed alongside the KG data.
  Changing the model produces a one-line diff in git that reviewers can see and approve.
- The amendment's legacy-key compatibility keeps older configs loadable without making
  `embed.device` or `embed.model` a second engine authority.
- `kkernel kg init` writes a valid, well-commented `.khive/config.toml` that makes its defaults
  explicit and reviewable in the initial PR.
- `--keep-existing` avoids recomputing vectors already present for the selected model and
  namespace.
- One selected config file, not a hidden per-key merge, reduces operator friction.

### Negative

- Inline create/update embedding adds model latency. On model-less or latency-sensitive servers,
  `kkernel mcp --no-embed` disables that work and a separate `kkernel reindex` process can run on
  an explicit schedule.
- `~/.khive/config.toml` introduces a user-global fallback that must be documented and
  supported. Model availability errors from lattice-embed are propagated at runtime.
- Changing `[[engines]]` requires re-embedding the affected corpus (potentially slow for large
  KGs). The explicit workflow is documented in §7.
- Under the amendment, changing entity input fields likewise requires explicit reindex;
  existing vectors are not silently rewritten when a config is edited.

### Neutral

- The NDJSON files and their git history are unchanged. This ADR adds no new committed
  artifacts beyond the selected config sections in `.khive/config.toml`.
- `working.db` already carries a per-(model, dim) vector table layout (ADR-005, ADR-009).
  This ADR specifies when those tables are populated, not how they are structured.
- Projects that do not use semantic retrieval can run `kkernel mcp --no-embed`; text search
  remains available, and `kkernel reindex` without a configured engine still backfills FTS.

## Open Questions

1. **`[embed.fields.include]` as a pack-level field.** For packs with non-standard entity
   schemas (e.g., a `lore` pack where atoms have `title` + `body` instead of `name` +
   `description`), a global `[embed.fields]` is too coarse. A future iteration may move
   embed field configuration under `[packs.*.embed_fields]`. The `[embed.fields]` section
   in this ADR is the v1 baseline for the common case; pack-level overrides are deferred.

2. **Per-namespace model selection.** Multi-namespace deployments may eventually need
   different models per namespace. `[[engines]]` remains the selected file's registry;
   the legacy `embed.model` key selects nothing. Namespace-scoped model selection
   is deferred until a real use case requires it.

3. **Legacy dimension comparison (resolved by the 2026-10-10 amendment).** Compare
   `embed.dimensions` only with the default engine's declared dimension. Do not initialize
   a model or defer a legacy mismatch warning until first embed. Provider identity and
   actual-dimension checks remain part of engine validation.

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md) — `embed.fields.include` cannot include
  `kind`; it is a closed-taxonomy discriminant, not an embeddable text field
- [ADR-005](ADR-005-storage-capability-traits.md) — `VectorStore` trait; `kkernel reindex`
  writes to per-(model, dim) tables via this trait
- [ADR-009](ADR-009-backend-architecture.md) — `khive-db` backend works in-memory and
  on-disk; `working.db` is a project-scoped on-disk backend
- [ADR-011](ADR-011-embedding-and-inference.md) — lattice-embed boundary used for batched
  reindex inference
- [ADR-020](ADR-020-git-native-kg-implementation.md) — git-native KG implementation and the
  NDJSON sync boundary; the `.khive/.gitignore` allowlist gains `config.toml`
- [ADR-028](ADR-028-pack-scoped-backends.md) — pack-scoped backends; `[[backends]]`,
  `[[engines]]`, and `[packs.*]` sections live in the same selected TOML file this ADR governs
- [ADR-031](ADR-031-multi-engine-retrieval.md) — `EmbedderRegistry`; `kkernel reindex`
  fans entity work across registered engines and applies note-kind embedding policy
  unless `--model` narrows it
- [ADR-034](ADR-034-kg-validation-pipelines.md) — validation pipelines remain separate from
  the explicit reindex maintenance path

## 2026-09-22 amendment — knowledge feedback ownership (#1781)

Knowledge feedback resolves live corpus atom/domain UUIDs and unique undashed
hex prefixes of at least 8 characters itself. Supplied target IDs are always
recorded; KG entity/note IDs and slugs are refused. The scalar `signal` and the
non-empty `section_signals` map are independently optional, but one is required.
A scalar signal requires a target; section-only feedback may omit it. The
caller-provided scalar is retained, never replaced with a synthetic useful vote.

`served_by_profile_id` takes precedence over pack configuration, followed by the
actor/namespace knowledge_compose binding, then the namespace-local section
prior. Profile/section learning requires an attributed caller. Section learning
uses a trusted Rust hook with opaque corpus attribution, no KG target resolution
and no wire verb. Its persisted event/log/snapshot updates only section state;
normal brain feedback continues to accept KG targets only.

Knowledge records its event before calling the profile hook. There is no
cross-backend atomic transaction. If profile persistence subsequently fails, the
error identifies the committed knowledge event and states that the profile
outcome is unconfirmed. Callers must inspect before retrying.

## 2026-09-29 amendment — note-kind embedding policy in reindex (#2227)

The comm pack's note-kind embedding policy governs both inline writes and an
unqualified `kkernel reindex` when that pack is selected. It declares `message`
as default-model-only; ordinary notes retain the all-models default. Reindex groups
notes by kind before embedding, including under `--keep-existing`, so it does not
fill secondary-model message rows during an ordinary repair pass. Entities still use
every registered model. An explicit `kkernel reindex --model <name>` selects that engine for
entities and notes and overrides the note-kind policy; knowledge retains its
default-engine behavior. An ordinary `kkernel reindex` does not itself migrate or
delete preexisting secondary-model message rows.

When an inline update removes an excluded model's historical vector, the cleanup
must match the row's stored model identity. Two model names may sanitize to one
table key; cleanup for the excluded name must preserve a selected model's row
and provenance in that table, including when replacement embedding fails.

## Amendment 2026-10-10: embedding pipeline and import strictness (#4785, #4786)

**Status: Accepted (2026-10-10).** Acceptance of the text is not implementation acceptance.
This section and the text it amends in §§1–3, 5, 7–9 and the affected rationale/consequences
specify intended behavior, not shipped support. Earlier dated amendments remain unchanged.
Embedding implementation follows landing the engine-configuration and runtime integration
prerequisites #5043 and #5050. Implementation acceptance requires the evidence below, executed
and reported in the implementation pull request.

### Scope and compatibility

The live pipeline keys are `embed.auto_embed`, `embed.batch_size` and
`embed.fields.include`. Legacy `model`, `dimensions` and `device` inside `[embed]` are
recognized and ignored after value validation, with only the §9 compatibility warning.
Unknown keys in the new tables warn and remain ignored; invalid known values abort with
file/key/source-location diagnostics. These are different cases, not a silent-default policy.

One selected config file remains authoritative. No per-key project/global merge, new
discovery tier, automatic migration or second engine selector is introduced. In particular,
`.khive/khive.toml` remains obsolete. Section 4's minimal canonical initializer remains
engine-only: new configs need not contain `[embed]` or `[schema]`. Existing configs containing
legacy keys remain compatible at the embed-table level, without waiving validation of the
rest of the file. An old explicitly written batch size of 64 remains 64; an omitted value
resolves to 128. The initializer's path, collision and atomic-create contract is unchanged.

For example, these optional additions to an otherwise valid selected config suppress
automatic serving embedding while permitting explicit reindex of entity names in batches
of 64. They do not replace the file's engine declarations:

```toml
# Pipeline settings; existing [[engines]] remains authoritative.
[embed]
auto_embed = false
batch_size = 64

[embed.fields]
include = ["name"]
```

An older `[embed] model = "default"` agrees silently only when `default` is the resolved
default engine's name. Adding `dimensions = 384` is silent only when that engine declares
384, or makes no comparison when it declares no dimension. A different declared dimension
warns without changing the engine. Any valid `device` setting warns; these reasons share one
startup compatibility warning. None causes model construction for comparison.

### Import policy boundary

This policy applies to explicit operator import through `kkernel kg import`, including
archive JSON and supported adapter/NDJSON inputs, and to `KhiveRuntime::import_kg` library
calls. `[schema] strict = true` is their default, including no section or an empty section.
It refuses unknown entity kinds and unknown relations before import writes begin. Setting
`strict = false` is a narrow **import-only** exception for unknown entity kinds: retain their spelling
and emit one warning per distinct unknown kind even without verbose output. Known aliases
and pack normalization retain their existing behavior. The exception does not add those
kinds to the registry or make an ordinary create accept them. These library import calls
retain strict defaults unless an explicit per-import policy is selected; this statement
does not change the separate VCS snapshot synchronization policy.

[ADR-001's accepted forward-compatibility rule](ADR-001-entity-kind-taxonomy.md#forward-compatibility-vcs-import)
requires an older-version VCS snapshot importer to downgrade an unknown kind to `Concept`,
clear `entity_type`, preserve the original kind/type metadata, add `khive:degraded_kind`
and warn. This amendment makes a **scoped supersession** of that rule for the explicit
operator/library import routes just named, even when their input bytes have snapshot
shape. VCS snapshot synchronization through `khive_vcs::run_sync` retains the ADR-001
degradation contract; `[schema].strict` does not select its policy. The
[ADR-001 companion](ADR-001-entity-kind-taxonomy.md#amendment-2026-10-10-explicit-import-and-vcs-snapshot-policy-boundary-4786),
accepted together with this amendment, records the same boundary.

The same otherwise-valid unknown-kind snapshot bytes therefore have three deliberately
different intended outcomes: explicit strict import refuses before writes; explicit
relaxed import retains the raw kind with its warning; older-version VCS synchronization
degrades under ADR-001. Choose by entry point and its explicit policy, not by guessing
provenance from JSON shape. The current synchronization validator rejects unknown kinds;
that divergence from the accepted degradation contract is an implementation gap, not
evidence that degradation already ships or authority to broaden this amendment's scope.

Unknown relations remain errors in both modes under the closed
[ADR-002 ontology](ADR-002-edge-ontology.md) and
[ADR-017 pack contract](ADR-017-pack-standard.md). Do not coerce, drop or hide an unknown
relation to make relaxed import appear successful. Other deterministic validation and
existing known-relation endpoint behavior remain unchanged. The preflight refusal guarantee
does not turn later storage/index operational failures into an all-or-nothing transaction.

### Required behavioral evidence

These are acceptance cases for subsequent implementation, **not executed results**.

| Case                                                                                                                                         | Required outcome                                                                                                                                                                               |
| -------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| No embed section                                                                                                                             | Automatic embedding enabled, reindex batch 128, fields `["name", "description"]`.                                                                                                              |
| Stored batch 64; explicit CLI batch                                                                                                          | Stored 64 is preserved; CLI value overrides it; effective maximum 500 and CLI zero-to-one clamp remain. `--id` with any explicit batch flag is refused.                                        |
| `auto_embed=false` at MCP, exec forwarding and daemon entry points                                                                           | Existing no-embed serving behavior, including no built-in startup backfill or query-vector fallback; custom-provider behavior unchanged.                                                       |
| Explicit no-embed with auto true                                                                                                             | Automatic embedding remains off; no new force-on override.                                                                                                                                     |
| Explicit reindex with auto false                                                                                                             | Same selected file, engine set, fields and namespace rules; note-kind policy remains unless `--model` overrides it.                                                                            |
| Automatic policy or ordered fields differ between client and daemon                                                                          | Forwarding identity cannot silently treat the policies as equivalent.                                                                                                                          |
| Legacy model/dimension agreement, disagreement or no declared dimension                                                                      | Agreement silent; disagreement gives the single compatibility warning; no declared dimension means no comparison or model loading. Auto false does not change the comparison engine.           |
| Any valid legacy device, including `cpu`                                                                                                     | One aggregated compatibility warning; no hardware selection.                                                                                                                                   |
| Unknown config key; invalid known value                                                                                                      | Unknown key warns and is ignored; invalid known value aborts with source diagnostics and no fallback to another file.                                                                          |
| Name-only input with a description sentinel                                                                                                  | Vector input excludes the sentinel; FTS still includes it under existing FTS rules.                                                                                                            |
| Ordered literal property selectors, including dotted keys                                                                                    | Preserve selector order and exact string bytes; missing/null/non-string values add no text; dots do not traverse nested objects.                                                               |
| Default input with empty name or description                                                                                                 | Preserve existing bytes, including the leading separator for empty name plus nonempty description and no trailing separator for empty description.                                             |
| Empty/blank/duplicate selectors or `kind`                                                                                                    | Config load fails.                                                                                                                                                                             |
| Whitespace-only selected input                                                                                                               | No embedding request or inserted vector; FTS proceeds and an existing vector is retained.                                                                                                      |
| Changed fields followed by reindex                                                                                                           | No automatic rewrite; keep-existing, failed replacement and healthy `--id` vector preservation remain; no promised purge for whitespace-only input.                                            |
| Strict import with valid records before a later unknown kind or relation                                                                     | Refusal before target writes; absent target remains absent and a seeded target remains unchanged.                                                                                              |
| Relaxed import with repeated and distinct unknown kinds                                                                                      | Import succeeds with spelling retained and one warning per distinct kind; get/export retains the imported kind; strict re-import refuses it.                                                   |
| Relaxed import with an unknown relation                                                                                                      | Refusal before writes; a valid closed-relation control retains existing behavior.                                                                                                              |
| Known aliases/pack kinds in either mode; ordinary create after relaxed import                                                                | Existing normalization remains; relaxed import does not change registry membership or ordinary create validation.                                                                              |
| The same otherwise-valid unknown-kind snapshot through explicit strict import, explicit relaxed import and older-version VCS synchronization | Strict import refuses before writes; relaxed import retains raw spelling; VCS synchronization follows ADR-001's complete degradation contract. Entry point selects policy, not document shape. |
| Invalid schema type or higher-precedence config                                                                                              | Structured config refusal before writes; no fallback to a lower-precedence file.                                                                                                               |

The relaxed-import acceptance cases require driven paired strict/relaxed inputs through the
actual import routes, including archive JSON and supported adapters. Controls must demonstrate
that unknown relations and other invalid records remain refused and leave targets unchanged.
Documented cases or static inspection alone do not supply that execution evidence. The
implementation pull request carries the executed results, including each refusal control's
output, before implementation acceptance. The cross-ADR boundary case additionally requires the
same snapshot-shaped input through all three routes with stored kind, subtype, metadata,
tag and warning observations. A source trace proving distinct call paths does not prove
their input domains are disjoint or satisfy the VCS degradation case.
