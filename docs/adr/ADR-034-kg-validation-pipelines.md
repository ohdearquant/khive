# ADR-034: KG Validation Pipelines

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive

## Context

[ADR-020](ADR-020-git-native-kg-implementation.md) introduced `kkernel kg validate` with
six built-in checks that guard the structural invariants git-native versioning depends on:
schema compliance, referential integrity, no duplicate UUIDs, sort order, remote resolution,
and cross-repo reference format. These checks are unconditional and apply to every KG.

Projects building domain-specific KGs need validation beyond these structural invariants.
A biology team requires all concept entities to carry a `taxa_rank` property. A research
team wants every entity to have at least three edges before it leaves a contributor's
branch. An organization enforcing naming conventions wants automated enforcement, not
manual review comments. None of these rules belong in the khive core — they are policy,
not structure.

The analogy to code linting is exact: ruff and ESLint are not part of the Python or
JavaScript runtimes, but every serious project uses them. They work because rules are
configurable, custom rules are first-class, and they integrate into normal commit and CI
workflows without modifying the language toolchain.

[ADR-023](ADR-023-declarative-pack-format.md) established that packs are a vocabulary
extension mechanism. Pack authors who define domain-specific entity kinds often know best
which invariants those kinds must satisfy — making the validation pipeline pack-aware
closes the loop between vocabulary and correctness enforcement.

This ADR extends `kkernel kg validate` to support:

1. A declarative rule configuration file at `.khive/kg/rules.yaml`.
2. A set of built-in configurable rules beyond the structural checks from ADR-020.
3. A custom rule API: Rust pack validators (v1); Deno/TypeScript executable rules deferred.
4. Pre-commit hook integration so validation runs before a commit can be created.
5. CI/CD integration with machine-readable output and a GitHub Action.
6. Auto-fix support for mechanically correctable violations.
7. Pack-provided rules shipped alongside the vocabulary they govern (ADR-023 extension).

### What changes and what does not

- ADR-020 `khive kg validate` built-in structural checks: **unchanged**. This ADR adds
  a second pass (the RulePass) that runs after the structural pass and is governed by
  `.khive/kg/rules.yaml`.
- ADR-023 pack standard: **extended via a Rust mechanism, not YAML.** Packs may
  contribute validation rules through a `const VALIDATION_RULES: &[ValidationRule]` on
  the `Pack` trait (see §7). Pack-contributed rules are merged into the active rule set
  at boot. Custom executable rules from non-Rust authors are NOT supported in v1 — the
  retracted YAML `pack.yaml` model is gone.
- ADR-020 `khive kg init`: **extended** to generate `.khive/kg/rules.yaml` and
  optionally install the pre-commit hook during project initialization.
- All other ADR-020 contracts: unchanged.

## Decision

### 1. Rule configuration

Rules are declared in `.khive/kg/rules.yaml`. The file is optional; if absent, only the
ADR-020 built-in structural checks run. When present, built-in checks are also
configurable through it.

```yaml
# .khive/kg/rules.yaml

rules:
  # Built-in rules from ADR-020 kkernel kg validate
  schema-compliance:
    severity: error
    enabled: true

  referential-integrity:
    severity: error
    enabled: true

  no-duplicate-uuids:
    severity: error
    enabled: true

  sort-order:
    severity: warning
    enabled: true

  remote-resolution:
    severity: error
    enabled: true # set false to skip network calls in offline environments
    resolve_remotes: false

  # Structural rules
  no-orphan-entities:
    severity: warning
    config:
      min_edges: 1 # every entity must have at least this many edges

  no-self-loops:
    severity: error

  # Density rules
  min-edge-density:
    severity: warning
    config:
      min_edges_per_entity: 3
      exclude_kinds: [person] # entity kinds exempt from the density check

  # Property rules
  required-properties:
    severity: error
    config:
      concept:
        - description
        - domain
      document:
        - title
        - authors
        - year

  # Naming rules
  naming-convention:
    severity: warning
    config:
      entity_names: title-case # "Flash Attention" not "flash attention"
      kind_names: lowercase # "concept" not "Concept"

  # Graph size
  max-entity-count:
    severity: info
    config:
      max: 10000
      message: "Consider splitting into multiple KGs"
```

Every rule entry has:

- **id** (the YAML key): stable identifier used in reports and fix invocations.
- **severity**: `error` | `warning` | `info`. Errors cause `kkernel kg validate` to exit
  with code 1. Warnings and info are printed but do not affect the exit code unless
  `--strict` is passed.
- **enabled**: `true` (default) | `false`. A disabled rule is not evaluated and produces
  no output.
- **config** (optional): rule-specific parameters. Unknown config keys for a built-in
  rule are an error in `rules.yaml` itself (not a KG violation), so typos surface
  immediately with a suggestion. Rules not listed in `rules.yaml` use their built-in
  defaults.

Built-in configurable rule set:

| Rule                    | Governs                                                                                 |
| ----------------------- | --------------------------------------------------------------------------------------- |
| `schema-compliance`     | Entity kinds and edge relations match `schema.yaml` vocabulary                          |
| `referential-integrity` | All edge targets resolve to known entities or valid remote refs                         |
| `no-duplicate-uuids`    | No two entity or edge records share a UUID                                              |
| `sort-order`            | `entities.ndjson` sorted by UUID; `edges.ndjson` sorted by `(source, target, relation)` |
| `remote-resolution`     | Cross-repo `<remote>:<uuid>` targets resolve at pinned commit                           |
| `no-orphan-entities`    | Every entity has at least `config.min_edges` edges                                      |
| `no-self-loops`         | No edge where `source == target`                                                        |
| `min-edge-density`      | Average edges per entity across entity kinds not in `exclude_kinds`                     |
| `required-properties`   | Per-kind required property key presence                                                 |
| `naming-convention`     | Entity name and kind name casing conventions                                            |
| `max-entity-count`      | Total entity count cap with a configurable advisory message                             |

### 2. Custom rule API

#### Custom executable rule runtimes (v1 scope)

v1 supports Rust pack validators only.

**Out of scope for v1** (deferred to a future ADR):

- Deno / TypeScript executable rules
- Non-Rust executable rule runtimes generally
- TS↔Rust FFI for validation
- Deno runtime packaging

v1 rule shape:

```rust
pub trait ValidationRule: Send + Sync {
    fn id(&self) -> RuleId;
    fn validate(
        &self,
        graph: &GraphSnapshot,
        ctx: &ValidationContext,
    ) -> Vec<ValidationFinding>;
}
```

Validation rules are registered through the Rust pack system (ADR-017). This avoids adding
a second executable runtime, permission model, sandbox story, packaging path, and failure
mode to v1.

**Rejected for v1**: Deno/TypeScript executable validation rules. Revisit in a follow-up ADR
once a downstream consumer presents a concrete requirement.

In v1, custom rules beyond the built-in set are authored as Rust pack validators (§9). The
`module` key in `rules.yaml` is reserved for a future non-Rust runtime and is not supported
in v1; specifying it produces exit code 2 ("unknown key `module` for custom rules — non-Rust
rule runtimes are not supported in v1").

### 3. Git hook integration

`kkernel kg init` generates `.khive/kg/hooks/pre-commit` and asks whether to install it:

```
kkernel kg init
  Initialized .khive/kg/ (schema.yaml, entities.ndjson, edges.ndjson)
  Install pre-commit hook? [y/N]: y
  Installed: .git/hooks/pre-commit -> .khive/kg/hooks/pre-commit
```

The hook script lives at `.khive/kg/hooks/pre-commit` so it is **tracked by git** alongside
the KG and rules. The `.git/hooks/pre-commit` entry is a symlink to the tracked script:

```bash
#!/usr/bin/env bash
# .khive/kg/hooks/pre-commit
# Generated by kkernel kg init.
# Runs KG validation on staged NDJSON files.
# Bypass with: git commit --no-verify

set -euo pipefail

staged=$(git diff --cached --name-only \
  | grep -E '^\.khive/kg/(entities|edges)\.ndjson$' || true)
if [ -z "$staged" ]; then
  exit 0
fi

kkernel kg validate
```

The hook runs only when `entities.ndjson` or `edges.ndjson` are staged, preventing false
positives on unrelated commits. Errors (exit code 1) block the commit. Warnings and info
do not block. `git commit --no-verify` bypasses the hook, consistent with git conventions.

Hook management subcommands (for repos without `init`):

| Subcommand                  | Behavior                                                             |
| --------------------------- | -------------------------------------------------------------------- |
| `kkernel kg hook install`   | Creates `.git/hooks/pre-commit` symlink to tracked hook script       |
| `kkernel kg hook uninstall` | Removes symlink; leaves `.khive/kg/hooks/pre-commit` intact          |
| `kkernel kg hook status`    | Shows whether symlink exists and whether it points to a valid target |

### 4. CLI flags

`kkernel kg validate` gains the following flags in addition to the existing
`--resolve-remotes` and `--schema-compat` from ADR-020:

| Flag                          | Behavior                                                           |
| ----------------------------- | ------------------------------------------------------------------ |
| `--fix`                       | Apply all fixable rules and report what changed                    |
| `--strict`                    | Treat warnings as errors; non-zero exit when `warnings > 0`        |
| `--format text\|json\|github` | Output format (default: `text`)                                    |
| `--verbose`                   | Expand all violation lists (default: show up to 2 then `+ N more`) |
| `--quiet`                     | Print summary line only; suppress per-rule lines                   |
| `--rules <path>`              | Override the default `.khive/kg/rules.yaml` path                   |
| `--no-rules`                  | Run ADR-020 built-in structural checks only; skip `rules.yaml`     |

### 5. Output formats

#### Text (default)

```
kkernel kg validate
  ✓ schema-compliance (420 entities, 1100 edges)
  ✓ referential-integrity
  ✓ no-duplicate-uuids
  ✓ sort-order
  ⚠ min-edge-density: 23 entities below threshold (min: 3 edges)
    - "FastSpeech2" (concept, id: 671b882a): 1 edge
    - "WaveGlow" (concept, id: 9a3c2b1d): 2 edges
    + 21 more  (run with --verbose to see all)
  ✗ required-properties: 5 entities missing required properties
    - "LoRA" (concept, id: c3f1a2b4): missing "domain"
    - "QLoRA" (concept, id: 88d7e6f5): missing "domain", "description"
    + 3 more

Summary: 1 error, 1 warning, 420 entities, 1100 edges
Exit code: 1
```

Symbols: `✓` = passed, `⚠` = warning, `✗` = error. These are text characters, not
decorative UI elements — they appear in terminal output and log files alike.

#### JSON (`--format json`)

```json
{
  "rules": [
    {
      "id": "schema-compliance",
      "severity": "error",
      "passed": true,
      "violations": []
    },
    {
      "id": "min-edge-density",
      "severity": "warning",
      "passed": false,
      "violations": [
        {
          "entity_id": "671b882a-1234-5678-abcd-ef0123456789",
          "entity_name": "FastSpeech2",
          "entity_kind": "concept",
          "rule_id": "min-edge-density",
          "severity": "warning",
          "message": "Entity has 1 edge (minimum: 3)",
          "fixable": false
        }
      ]
    }
  ],
  "summary": {
    "errors": 0,
    "warnings": 2,
    "info": 0,
    "entities": 420,
    "edges": 1100,
    "passed": true
  }
}
```

`summary.passed` is `true` when `errors == 0`. With `--strict`, `passed` is `true` only
when `errors == 0 && warnings == 0`.

#### GitHub Actions (`--format github`)

Emits `::error file=...::` and `::warning file=...::` annotations so violations surface
inline in the PR diff view. No output for passing rules.

### 6. Exit codes

| Code | Meaning                                                                                                            |
| ---- | ------------------------------------------------------------------------------------------------------------------ |
| `0`  | All rules passed (no errors; warnings allowed unless `--strict`)                                                   |
| `1`  | One or more rules at severity `error` violated                                                                     |
| `2`  | `rules.yaml` itself failed schema validation (malformed, unknown key, missing module file, invalid severity value) |

Exit code 2 is reserved for infrastructure failures in the rules file. CI pipelines can
route exit 1 (fix your KG) and exit 2 (fix your rules file) to different notifications.

### 7. Auto-fix

`kkernel kg validate --fix` applies all fixable rules and reports what changed:

```
kkernel kg validate --fix
  ✓ schema-compliance
  ~ sort-order: re-sorted entities.ndjson (3 lines moved)
  ~ naming-convention: normalized 5 entity names to title-case
    - "flash attention" -> "Flash Attention" (concept, id: 4b2a1c3d)
    - "lora" -> "LoRA" (concept, id: c3f1a2b4)
    + 3 more
  ✗ required-properties: 5 entities missing required properties (cannot auto-fix)

Summary: 1 error fixed, 1 error unfixable, 420 entities, 1100 edges
```

`--fix` writes to `entities.ndjson` and `edges.ndjson` in place. Files are only written if
at least one fixable violation was found. The pre-commit hook does not run `--fix`
automatically; the contributor must run it explicitly.

Built-in fixable rules:

| Rule                                 | Fix behavior                                       |
| ------------------------------------ | -------------------------------------------------- |
| `sort-order`                         | Re-sorts both NDJSON files in canonical sort order |
| `naming-convention` (`entity_names`) | Normalizes entity names to title-case per config   |

Built-in unfixable rules (require human judgment):

| Rule                  | Why unfixable                                                  |
| --------------------- | -------------------------------------------------------------- |
| `required-properties` | The missing value must come from the contributor               |
| `min-edge-density`    | Which edges to add is a semantic decision                      |
| `no-orphan-entities`  | Whether to add edges or delete the entity is context-dependent |

Pack-provided rules (§9) may also declare auto-fix by providing a `fix` field in
`ValidationRule`:

```rust
ValidationRule {
    id:       "biology/normalize-taxa-rank",
    severity: Severity::Warning,
    check:    check_taxa_rank as RuleFn,
    fix:      Some(fix_taxa_rank as FixFn),
    ..
}
```

The `fix` callback receives the same `ValidationContext` and the violations emitted by
`check`, and returns a `GraphPatch` applied by the validator before writing NDJSON.

### 8. Rule evaluation order

The validation pipeline runs in a defined sequence:

1. **ADR-020 structural checks** (schema compliance, referential integrity, duplicate
   UUIDs, sort order, remote resolution). Run first; results always included even if later
   passes fail.
2. **Built-in configurable rules** from `rules.yaml` without a `module` key (orphan
   entities, self-loops, edge density, required properties, naming, max count).
3. **Custom rules** from `rules.yaml` with a `module` key, in file declaration order.
4. **Pack-provided rules** (§9), in pack installation order from `schema.yaml#packs`.

A structural error in step 1 does not abort steps 2–4. All passes run to completion so
contributors receive a full picture in a single invocation rather than iterative
fix-and-validate cycles.

### 9. Pack-provided rules

A pack ([ADR-023](ADR-023-declarative-pack-format.md)) declares validation rules as a
`const` on its `Pack` impl. Each rule is a Rust struct carrying the same identity,
severity, and predicate logic as a project custom rule (§2), but compiled into the pack
binary — no separate `validation/` directory, no `pack.yaml`.

```rust
// crates/khive-pack-biology/src/lib.rs
use khive_runtime::validation::{ValidationRule, Severity, RuleFn};

impl Pack for BiologyPack {
    const NAME:         &'static str            = "biology";
    const ENTITY_KINDS: &'static [&'static str] = &["species"];
    // ... other consts ...

    const VALIDATION_RULES: &'static [ValidationRule] = &[
        ValidationRule {
            id:          "biology/required-taxa-rank",
            severity:    Severity::Warning,
            description: "Requires all species entities to carry a taxa_rank property",
            check:       check_required_taxa_rank as RuleFn,
            fix:         None,
        },
    ];
}

fn check_required_taxa_rank(ctx: &ValidationContext) -> Vec<Violation> { ... }
```

Pack rule IDs are namespaced by pack name to prevent collisions:
`<pack-name>/<rule-id>`. In the example above the full rule ID is
`biology/required-taxa-rank`. Projects can override a pack rule's severity or disable it
in their `rules.yaml`:

```yaml
rules:
  biology/required-taxa-rank:
    severity: error # escalate from pack default of warning
    enabled: true
```

Pack rules not mentioned in `rules.yaml` run with the severity declared in the pack's
`VALIDATION_RULES` const. Installing a pack may add new validation rules to the
project's pipeline without any `rules.yaml` change — the pack author signals intended
severity in code.

**No executable TypeScript rules from packs in v1.** Pack rules are Rust functions
compiled into the kkernel binary; their trust model is "trusted by compilation"
(ADR-023). Custom executable rules in non-Rust languages are reserved for a future ADR
once the security/sandboxing model is settled.

### 9a. Rule shapes — `CorpusCheck` vs streaming `Fold`

The `RuleFn = fn(&ValidationContext) -> Vec<Violation>` shape used by `VALIDATION_RULES`
above is a **whole-corpus check** — the rule sees `entities + edges + schema + config`
together and returns violations. This is the right shape for rules that span the corpus
(referential integrity, remote resolution, min-edge-density, cross-repo references).

Some rules don't need the whole corpus — they evaluate one record at a time. For these,
ADR-024's `Fold` is a better fit because it streams, has reusable combinators
(`FilterFold`, `MapFold`), and runs deterministically over `EventStore`/`EntityStore`
output without materializing the whole corpus in memory.

Two complementary rule shapes are supported. Both return `Vec<Violation>` per rule
invocation — the validator aggregates per-rule violations into the final
`ValidationReport`. `CorpusCheck::check` matches the existing `RuleFn = fn(&ValidationContext)
-> Vec<Violation>` shape so pack-declared rules (§9) and the streaming dispatcher
agree on the per-rule contract.

```rust
/// Whole-corpus check (existing). Sees entities + edges + schema together.
pub trait CorpusCheck: Send + Sync {
    fn check(&self, ctx: &ValidationContext) -> Vec<Violation>;
}

/// Streaming check (new). Per-entity, per-edge, or per-event reduction.
/// Reuses ADR-024 Fold combinators. The item enum is owned because the dispatcher
/// streams records out of the stores by value — there is no corpus-wide borrow
/// to tie the items to. Cheap clones (entity/edge/event records are small
/// structs); Arc-wrap if pack authors need shared ownership.
pub enum ValidationItem {
    Entity(Entity),
    Edge(Edge),
    Event(Event),
}

pub trait StreamingRule: Fold<ValidationItem, RuleState> + Send + Sync {
    /// Inherited from Fold: init, reduce, finalize, derive.
    /// finalize() converts accumulated state into the rule's RuleState; the
    /// dispatcher calls to_violations() on the finalized state.
    fn to_violations(&self, state: RuleState) -> Vec<Violation>;
}
```

The validator dispatches each rule by shape and unifies on `Vec<Violation>`:

```
For each declared rule:
  let vs: Vec<Violation> = match rule.shape() {
    Corpus    => rule.as_corpus_check().check(ctx),                         // one batch
    Streaming => {
        let state = stream_items(stores)                                    // per-record
            .fold(rule.init(&fold_ctx),
                  |s, item| rule.reduce(s, &item, &fold_ctx));
        let state = rule.finalize(state, &fold_ctx);
        rule.to_violations(state)
    }
  };
  report.add(rule.id(), vs);
Aggregate per-rule entries → ValidationReport.
```

**Determinism for streaming reports**: rules MUST emit violations in canonical order —
`BTreeSet` / `BTreeMap` / `Vec + final sort`, never `HashMap` iteration order. Canonical
violation order across the report:

```
(rule_id ASC, severity DESC, entity_id ASC NULLS LAST, edge_id ASC NULLS LAST, message ASC)
```

**When to choose which shape**:

All streaming rules implement `StreamingRule` (which `: Fold<ValidationItem, RuleState>`).
The "what's inside the ValidationItem" column says which variants the rule's `reduce`
actually inspects — the dispatcher routes all three variants to every streaming rule,
and the rule's body matches on the variant it cares about. Pure counters and per-kind
filters are cheap; pack authors can compose `FilterFold` (ADR-024) to discard variants
they don't need.

| Rule                                | Shape                                    | Inspects                 | Reason                          |
| ----------------------------------- | ---------------------------------------- | ------------------------ | ------------------------------- |
| Required property present on entity | `StreamingRule`                          | `ValidationItem::Entity` | Per-entity, no joins            |
| Naming convention                   | `StreamingRule`                          | `ValidationItem::Entity` | Per-entity                      |
| No duplicate UUIDs                  | `StreamingRule` (state: `HashSet<Uuid>`) | `ValidationItem::Entity` | Accumulator                     |
| No self-loops                       | `StreamingRule`                          | `ValidationItem::Edge`   | Per-edge                        |
| Max entity count                    | `StreamingRule` (state: counter)         | `ValidationItem::Entity` | Pure counter                    |
| Referential integrity               | `CorpusCheck`                            | n/a                      | Needs entities + edges together |
| Min edge density                    | `CorpusCheck`                            | n/a                      | Aggregate over the whole corpus |
| Remote resolution                   | `CorpusCheck`                            | n/a                      | Needs remote registry config    |

Pack authors may declare both shapes in `VALIDATION_RULES` — the runtime selects by
trait impl at boot. The `Severity`, `id`, `description`, `fix` fields are identical;
only the predicate side differs.

### 10. `rules.yaml` schema validation

`kkernel kg validate` validates `rules.yaml` itself against a built-in JSON Schema before
evaluating any rules. A malformed `rules.yaml` — unknown top-level key, invalid severity
value, `module` pointing to a non-existent file, unknown `config` key for a built-in rule
— produces a structured error naming the offending field and aborts with exit code 2:

```
ERROR: rules.yaml line 14: unknown config key "min_edges_per_node" for rule "min-edge-density"
  Did you mean "min_edges_per_entity"?
```

This validation is separate from and prior to KG validation. Exit code 2 is distinct from
exit code 1 (KG violations) so CI can route the two failure modes differently.

### 11. CI/CD integration

A GitHub Action `khive/kg-validate-action@v1` wraps `kkernel kg validate --format github`
for use in PR workflows. `kkernel kg init --ci` generates `.github/workflows/kg-validate.yml`:

```yaml
name: KG Validate
on:
  push:
    paths: [".khive/kg/**"]
  pull_request:
    paths: [".khive/kg/**"]

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Validate KG
        uses: khive/kg-validate-action@v1
        with:
          rules: .khive/kg/rules.yaml
          fail-on: error # "error" | "warning" | "never"
          format: github
          resolve-remotes: "true"
```

The `format: github` output surfaces violations as inline PR diff annotations. The
`fail-on: warning` option maps to `--strict`. The action is a separate repository
publishable independently of CLI version releases.

## Rationale

### Why a second pass rather than extending the structural checks

The ADR-020 structural checks guard invariants the git versioning layer depends on — sort
order, referential integrity, schema compliance. They must run unconditionally and produce
the same results on every KG regardless of project configuration. Project-level policy
(required properties, naming conventions, edge density) must be configurable and optional.
Mixing them would make the structural checks depend on `rules.yaml`, creating a startup
ordering problem and coupling the storage layer to project policy.

A second pass (RulePass after StructuralPass) is the clean separation: the structural
layer is configuration-free; the rule layer reads policy from `rules.yaml`.

### Why `.khive/kg/rules.yaml` rather than inline NDJSON annotations

Validation policy does not belong in the data layer. The NDJSON entity and edge files are
the interchange format — any tool that understands NDJSON can consume a khive KG. Adding
inline rule annotations would couple the validation system to the data format and break
ADR-020's goal of a clean interchange format. `rules.yaml` is the policy layer; NDJSON
is the data layer. The separation is the same principle that keeps ESLint config out of
JavaScript source files.

### Why Rust-only for custom rules in v1

Non-Rust executable rule runtimes (Deno, WASM, subprocess) each require a second trust
boundary: a permission model, a sandbox story, a packaging path, and a new failure mode.
None of that cost is justified without a concrete downstream consumer requiring it.

v1 custom rules are Rust pack validators compiled into the `kkernel` binary. Their trust
model is "trusted by compilation" — the same model as the pack vocabulary itself (ADR-017).
No additional sandbox is needed.

Deno/TypeScript executable rules are explicitly deferred. The `module` key in `rules.yaml`
is reserved but unimplemented in v1; the validator rejects it with exit code 2. A follow-up
ADR may activate the key once a consumer presents a concrete requirement.

### Why per-rule severity rather than per-violation severity for built-ins

Per-rule severity is the model used by ruff, ESLint, and every mature linter. Rules produce
violations of one severity — the tool author decides whether a class of violation is an
error or a warning, and the project configures it. Per-violation severity is supported in
the custom rule API (the rule author controls the `Violation` objects returned) and in
pack rules, which can emit mixed-severity output within a single rule. For built-in rules
configured through `rules.yaml`, a single severity scalar is what projects want to set.

### Why pack-provided rules use a namespace prefix

Without namespace prefixes, a pack named `biology` and a project custom rule could both
declare a rule named `required-taxa-rank`. The collision would be silent and produce
confusing behavior in reports. The `<pack-name>/` prefix eliminates the ambiguity and makes
rule provenance visible in violation output and `rules.yaml` overrides.

### Why auto-fix is opt-in (`--fix`) rather than automatic

Automatic fixes during validation create a non-idempotent operation: a validation run that
also modifies files means the output state is not predictable from the input state alone.
Pre-commit hooks should report violations and exit, not silently transform files. The
contributor decides whether to apply the fix, reviews the diff, and commits deliberately.
`--fix` is explicit, reports what changed, and leaves the commit decision to the human.

### Why exit code 2 for `rules.yaml` errors

CI pipelines benefit from distinguishing "the KG has violations" (fix your data, exit 1)
from "the rules file is malformed" (fix your configuration, exit 2). These require
different remediators — a contributor versus an infrastructure maintainer. Conflating them
into a single non-zero exit code obscures which action is needed.

## Alternatives Considered

| Alternative                                 | Pros                            | Cons                                                                         | Decision                                                           |
| ------------------------------------------- | ------------------------------- | ---------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| No custom rules, only built-in configurable | Simple, zero config             | Cannot express domain constraints; biology example alone shows insufficiency | Rejected                                                           |
| JSON Schema for property validation only    | Widely known tooling            | Cannot express structural rules (density, orphans, topology)                 | Partial — `required-properties` rule implicitly covers this case   |
| WASM plugins for custom rules               | Language-agnostic, performance  | Complex build chain; adds second executable runtime and trust boundary       | Deferred — no concrete consumer in v1                              |
| Deno/TypeScript custom rules                | Type-safe, contributor-friendly | Requires embedded Deno runtime, permission model, packaging path, sandbox    | Rejected for v1; revisit in a follow-up ADR with concrete use-case |
| Server-side validation only                 | No local tooling                | Breaks local-first, offline, CI reproducibility guarantees from ADR-020      | Rejected; local-first is a hard requirement                        |
| Inline NDJSON rule annotations              | Colocation of data and policy   | Couples validation policy to interchange format                              | Rejected; data/policy separation is explicit design intent         |
| Global `~/.khive/kg/rules.yaml`             | One file for all projects       | Projects have different ontologies; team rules should travel with the repo   | Rejected; per-project file is git-tracked and PR-reviewable        |
| Automatic fix on validation                 | Zero extra command              | Non-idempotent; silent file modification in hooks                            | Rejected; `--fix` is explicit and deliberate                       |

## Consequences

### Positive

- Teams enforce domain-specific invariants (required properties, naming, edge density)
  without touching khive core.
- Violations surface at commit time via the pre-commit hook, before a PR is opened.
- CI produces inline PR diff annotations via `--format github`.
- `--fix` reduces friction for mechanical violations (sort order, naming normalization).
- Pack authors bundle rules alongside their vocabulary, colocating domain constraints with
  the vocabulary they govern.
- `rules.yaml` schema validation with error messages surfaces misconfiguration immediately
  with exit code 2, distinct from KG violations (exit code 1).

### Negative

- Custom rules in v1 require authoring in Rust and compiling a pack. Contributors who want
  a lightweight scripted rule cannot use TypeScript/Deno until a future ADR activates that
  runtime.
- `rules.yaml` is a new file teams must maintain. Projects that only need ADR-020 built-in
  checks can omit it entirely; the maintenance cost is zero unless opted in.
- `--fix` for `naming-convention` modifies entity names. If entity names are referenced in
  external documentation or cross-repo edges in other repositories, renaming them creates
  a consistency gap. Contributors should review fix output before committing.

### Neutral

- The ADR-020 structural pass is unchanged. Existing `kkernel kg validate` invocations
  continue to work and gain the new rules transparently.
- The JSON output format (`--format json`) extends the ADR-020 exit-code contract: 0 for
  clean, 1 for violations, 2 for `rules.yaml` parse errors. The text format is a superset
  of the ADR-020 single-line-per-check output.
- `kkernel kg init` is extended but backward-compatible. Existing `.khive/kg/` directories
  are unaffected; `kkernel kg init --add-hooks` installs the hook without reinitializing.

## Open Questions

1. **Non-Rust rule runtimes**: When a downstream consumer presents a concrete requirement
   for Deno/TypeScript or WASM executable rules, a follow-up ADR should define: module
   format contract, permission model, sandbox story, packaging path, and failure modes.
   The `module` key in `rules.yaml` is reserved for that future ADR.

2. **`rules.yaml` inheritance**: Should projects be able to extend a shared `rules.yaml`
   (e.g., from an organization's pack) rather than declaring all rules from scratch? An
   `extends:` key at the top level is the natural shape; deferred pending demand.

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md): Entity kind taxonomy — entity kinds
  referenced in `required-properties` config entries
- [ADR-002](ADR-002-edge-ontology.md): Edge ontology — closed edge relation set validated
  by structural and custom rules
- [ADR-013](ADR-013-note-kind-taxonomy.md): Note kind taxonomy — note kinds in property
  configuration contexts
- [ADR-020](ADR-020-git-native-kg-implementation.md): Git-native KG implementation —
  defines `kkernel kg validate` built-in structural checks that this ADR's RulePass extends;
  `kkernel kg init` and `kkernel kg hook` commands; CI workflow generation
- [ADR-023](ADR-023-declarative-pack-format.md): Declarative pack format — `pack.yaml`
  manifest extended by this ADR's `validation:` section; pack installation lifecycle
- ESLint configuration reference: <https://eslint.org/docs/latest/use/configure/>
- ruff configuration reference: <https://docs.astral.sh/ruff/configuration/>
