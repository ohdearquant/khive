# ADR-034: KG Validation Pipelines

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers

## Context

[ADR-020](./ADR-020-git-native-kg-implementation.md) introduced `kkernel kg validate` with
six built-in checks that guard the structural invariants git-native versioning depends on:
schema compliance, referential integrity, no duplicate UUIDs, sort order, remote resolution,
and cross-repo reference format. These checks are unconditional and apply to every KG.

Projects building domain-specific KGs need validation beyond these structural invariants.
A biology team requires all concept entities to carry a `taxa_rank` property. A research
team wants every entity to have at least three edges before it leaves a contributor's
branch. An organization enforcing naming conventions wants automated enforcement, not
manual review comments. None of these rules belong in the khive core: they are policy,
not structure.

The analogy to code linting is exact: ruff and ESLint are not part of the Python or
JavaScript runtimes, but every serious project uses them. They work because rules are
configurable, custom rules are first-class, and they integrate into normal commit and CI
workflows without modifying the language toolchain.

[ADR-023](./ADR-023-declarative-pack-format.md) established that packs are a vocabulary
extension mechanism. Pack authors who define domain-specific entity kinds often know best
which invariants those kinds must satisfy: making the validation pipeline pack-aware
closes the loop between vocabulary and correctness enforcement.

This ADR documents the shipped `kkernel kg validate` validation surface:

1. An optional TOML rule configuration file at `.khive/kg/rules.toml`.
2. ADR-020 structural checks that always run first.
3. A TOML `[[rules]]` RulePass with `id`, `severity`, `kind`, optional `condition`,
   optional `require_field`, and optional `message` fields.
4. Pre-commit hook integration so validation can run before a commit can be created.
5. CI/CD integration with machine-readable output.
6. Unsupported YAML rule files are rejected with an error directing users to TOML.
7. Custom executable rules (Deno/TypeScript) and YAML schema validation are not shipped.
   Pack-provided validators have a shipped Rust API (`PackRuntime::validation_rules`,
   `VerbRegistry::all_validation_rules`) but CLI runner integration remains deferred.
   Auto-fix callbacks have a type stub (`GraphPatch`) but no write-path implementation.

### What changes and what does not

- ADR-020 `khive kg validate` built-in structural checks: **unchanged**. This ADR adds
  an optional TOML RulePass that runs after the structural pass when
  `.khive/kg/rules.toml` exists and `--no-rules` is not set.
- ADR-023 pack standard: **extended by the shipped Rust validator API** (`PackRuntime::validation_rules`,
  `VerbRegistry::all_validation_rules`). What remains deferred is CLI runner integration:
  `kkernel kg validate` does not yet call these methods against live corpus data.
- ADR-020 `khive kg init`: currently initializes `.khive/kg/{entities,edges}.ndjson`,
  `.khive/khive.toml`, and hooks; it does not generate `rules.toml`.
- All other ADR-020 contracts: unchanged.

## Decision

### 1. Rule configuration

Rules are declared in `.khive/kg/rules.toml`. The file is optional; if absent, only the
ADR-020 built-in structural checks run. When present and `--no-rules` is not set,
the validator parses TOML and evaluates each `[[rules]]` entry in file order.

```toml
# .khive/kg/rules.toml

[[rules]]
id = "concept-must-have-description"
severity = "warning"
kind = "entity"
condition = "kind=concept"
require_field = "description"
message = "Concept {id} missing description"

[[rules]]
id = "no-self-loops"
severity = "error"
kind = "edge"
condition = "source_id=target_id"
message = "Self-loop detected on {id}"
```

Every shipped TOML rule entry has:

- **id**: stable identifier used in `RuleResult.id`.
- **severity**: `error` | `warning` | `info`. Errors cause `kkernel kg validate` to exit
  with code 1. Warnings and info are printed but do not affect the exit code unless
  `--strict` is passed.
- **kind**: `entity` or `edge`; unknown kinds produce an error rule result.
- **condition**: optional `field=value` equality filter; `source_id=target_id` is the
  self-loop sentinel for edges.
- **require_field**: optional field name that must be present and non-empty on matching
  records.
- **message**: optional violation message; `{id}` is replaced with the record id when
  present.

The shipped TOML schema has no `enabled`, `module`, or nested `config` fields. Remove a
rule entry to disable it.

### 2. Configurable TOML RulePass

The shipped configurable pass is data-driven from `.khive/kg/rules.toml`, not a Rust
custom-rule API. Each `[[rules]]` entry selects `kind = "entity"` or `"edge"`, optionally
filters by `condition`, optionally requires a field with `require_field`, and emits
`message` with `{id}` substitution when a matching record violates the rule.

The Rust pack validator API (`PackRuntime::validation_rules`, `VerbRegistry::all_validation_rules`,
`ValidationRule`, `ValidationContext`, `Violation`) is shipped in `khive-runtime`. What is
deferred is wiring this API into the `kkernel kg validate` CLI runner so that pack-provided
rules execute against corpus data. Deno/TypeScript executable rules, a `module` key, and
non-Rust rule runtimes remain deferred pending a follow-up ADR.

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

| Flag                          | Behavior                                                                     |
| ----------------------------- | ---------------------------------------------------------------------------- |
| `--fix`                       | Apply all fixable rules and report what changed                              |
| `--strict`                    | Treat warnings as errors; non-zero exit when `warnings > 0`                  |
| `--format text\|json\|github` | Output format (default: `text`)                                              |
| `--verbose`                   | Expand all violation lists (default: show up to 2 then `+ N more`)           |
| `--quiet`                     | Print summary line only; suppress per-rule lines                             |
| `--rules <path>`              | Override the default `.khive/kg/rules.toml` path                             |
| `--no-rules`                  | Run ADR-020 built-in structural checks only; skip configurable TOML RulePass |

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
    - "Example Technique" (concept, id: c3f1a2b4): missing "domain"
    - "Example Variant" (concept, id: 88d7e6f5): missing "domain", "description"
    + 3 more

Summary: 1 error, 1 warning, 420 entities, 1100 edges
Exit code: 1
```

Symbols: `✓` = passed, `⚠` = warning, `✗` = error. These are text characters, not
decorative UI elements: they appear in terminal output and log files alike.

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

| Code | Meaning                                                                                         |
| ---- | ----------------------------------------------------------------------------------------------- |
| `0`  | All rules passed (no errors; warnings allowed unless `--strict`)                                |
| `1`  | One or more rules at severity `error` violated                                                  |
| `2`  | `rules.toml` parse failure or unsupported format (e.g. `.yaml`/`.yml` file passed to `--rules`) |

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
    - "example technique" -> "Example Technique" (concept, id: c3f1a2b4)
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

The pack validator API reserves a `fix` field on `ValidationRule`:

```rust
ValidationRule {
    id:       "biology/normalize-taxa-rank",
    severity: Severity::Warning,
    check:    check_taxa_rank as RuleFn,
    fix:      Some(fix_taxa_rank as FixFn),
    ..
}
```

The intended callback receives the same `ValidationContext` and the violations emitted by
`check`. This callback is not active in the CLI: `GraphPatch` is currently an empty stub,
and pack-provided rules are not yet connected to the CLI runner, as specified in §9.

### 8. Rule evaluation order

The validation pipeline runs in a defined sequence:

1. **ADR-020 structural checks** (schema compliance, referential integrity, duplicate
   UUIDs, sort order, remote resolution). Run first; results always included.
2. **Configurable TOML rules** from `.khive/kg/rules.toml`, when the file exists and
   `--no-rules` is not set. Rules run in file order.
3. **Pack-provided validation rules** (§9): the Rust API is shipped; CLI runner integration
   is deferred. **Custom executable rules** (Deno/TypeScript) are not shipped.

A structural error in step 1 does not abort the optional TOML RulePass. All shipped passes
run to completion so contributors receive a full picture in a single invocation rather than
iterative fix-and-validate cycles.

### 9. Pack-provided validator API

The Rust validator API is shipped in `khive-runtime`:

- `crates/khive-runtime/src/validation.rs` defines `ValidationRule`, `RuleFn`, `FixFn`,
  `GraphPatch`, `ValidationContext`, `GraphSnapshot`, `Violation`, `ValidationReport`, and
  `Severity`.
- `PackRuntime::validation_rules() -> &'static [ValidationRule]` (in
  `crates/khive-runtime/src/pack.rs`) is a trait method packs override to contribute
  domain-specific rules. The default returns `&[]` so existing packs compile without changes.
- `VerbRegistry::all_validation_rules()` (in `crates/khive-runtime/src/pack.rs`) collects
  rules from every registered pack and returns them as `Vec<&'static ValidationRule>`.

Rule IDs must follow the `<pack>/<rule-id>` namespace convention. Built-in rules (no pack
prefix) are reserved for the `khive-runtime` validation infrastructure.

**What remains deferred**: the CLI runner (`kkernel kg validate`) does not yet call
`all_validation_rules()` against live corpus data. The `GraphPatch` type carries no fields
(auto-fix callbacks are a stub pending the write path). `GraphSnapshot` exposes only
`entity_count` and `edge_count` in v1. A follow-up ADR must wire the runtime rule
collection into the CLI validation pass and extend `GraphSnapshot` before pack-provided
rules run during `kkernel kg validate` invocations.

### 10. `rules.toml` loading and unsupported YAML

`kkernel kg validate` parses `rules.toml` with TOML deserialization. Files ending in
`.yaml` or `.yml` are rejected with an error directing the user to TOML. A rule-file parse
or unsupported-format error aborts with exit code 2:

```
ERROR: rules.toml: TOML parse error at line 3, column 1: expected a table key, found '.'
ERROR: rules.yaml: unsupported rules file format: rename to rules.toml and convert to TOML [[rules]] syntax
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
          rules: .khive/kg/rules.toml
          fail-on: error # "error" | "warning" | "never"
          format: github
          resolve-remotes: "true"
```

The `format: github` output surfaces violations as inline PR diff annotations. The
`fail-on: warning` option maps to `--strict`. The action is a separate repository
publishable independently of CLI version releases.

## Rationale

### Why a second pass rather than extending the structural checks

The ADR-020 structural checks guard invariants the git versioning layer depends on: sort
order, referential integrity, schema compliance. They must run unconditionally and produce
the same results on every KG regardless of project configuration. Project-level policy
(required properties, naming conventions, edge density) must be configurable and optional.
Mixing them would make the structural checks depend on `rules.toml`, creating a startup
ordering problem and coupling the storage layer to project policy.

A second pass (RulePass after StructuralPass) is the clean separation: the structural
layer is configuration-free; the rule layer reads policy from `rules.toml`.

### Why `.khive/kg/rules.toml` rather than inline NDJSON annotations

Validation policy does not belong in the data layer. The NDJSON entity and edge files are
the interchange format: any tool that understands NDJSON can consume a khive KG. Adding
inline rule annotations would couple the validation system to the data format and break
ADR-020's goal of a clean interchange format. `rules.toml` is the policy layer; NDJSON
is the data layer. The separation is the same principle that keeps ESLint config out of
JavaScript source files.

### Why Rust-only for custom rules in v1

Non-Rust executable rule runtimes (Deno, WASM, subprocess) each require a second trust
boundary: a permission model, a sandbox story, a packaging path, and a new failure mode.
None of that cost is justified without a concrete downstream consumer requiring it.

The shipped extension API is limited to Rust pack validators compiled into `kkernel`. Its
trust model is "trusted by compilation," the same model as pack vocabulary (ADR-017). The
CLI runner does not execute these validators yet, so the current configurable execution
surface remains the TOML RulePass.

Deno/TypeScript executable rules are explicitly deferred. The shipped TOML `RuleConfig`
has no `module` field. Non-Rust executable rules remain future work and are not part of
the current validation surface.

### Why per-rule severity rather than per-violation severity for built-ins

Per-rule severity is the model used by ruff, ESLint, and every mature linter. Rules produce
violations of one severity: the tool author decides whether a class of violation is an
error or a warning, and the project configures it. Per-violation severity is supported in
the custom rule API (the rule author controls the `Violation` objects returned) and in
pack rules, which can emit mixed-severity output within a single rule. For built-in rules
configured through `rules.toml`, a single severity scalar is what projects want to set.

### Why pack-provided rules use a namespace prefix

Without namespace prefixes, a pack named `biology` and a project custom rule could both
declare a rule named `required-taxa-rank`. The collision would be silent and produce
confusing behavior in reports. The `<pack-name>/` prefix eliminates the ambiguity and makes
rule provenance visible in violation output and `rules.toml` overrides.

### Why auto-fix is opt-in (`--fix`) rather than automatic

Automatic fixes during validation create a non-idempotent operation: a validation run that
also modifies files means the output state is not predictable from the input state alone.
Pre-commit hooks should report violations and exit, not silently transform files. The
contributor decides whether to apply the fix, reviews the diff, and commits deliberately.
`--fix` is explicit, reports what changed, and leaves the commit decision to the human.

### Why exit code 2 for rule-file errors

CI pipelines benefit from distinguishing "the KG has violations" (fix your data, exit 1)
from "the rules file is malformed" (fix your configuration, exit 2). These require
different remediators: a contributor versus an infrastructure maintainer. Conflating them
into a single non-zero exit code obscures which action is needed.

## Alternatives Considered

| Alternative                                 | Pros                            | Cons                                                                         | Decision                                                           |
| ------------------------------------------- | ------------------------------- | ---------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| No custom rules, only built-in configurable | Simple, zero config             | Cannot express domain constraints; biology example alone shows insufficiency | Rejected                                                           |
| JSON Schema for property validation only    | Widely known tooling            | Cannot express structural rules (density, orphans, topology)                 | Partial: `required-properties` rule implicitly covers this case    |
| WASM plugins for custom rules               | Language-agnostic, performance  | Complex build chain; adds second executable runtime and trust boundary       | Deferred: no concrete consumer in v1                               |
| Deno/TypeScript custom rules                | Type-safe, contributor-friendly | Requires embedded Deno runtime, permission model, packaging path, sandbox    | Rejected for v1; revisit in a follow-up ADR with concrete use-case |
| Server-side validation only                 | No local tooling                | Breaks local-first, offline, CI reproducibility guarantees from ADR-020      | Rejected; local-first is a hard requirement                        |
| Inline NDJSON rule annotations              | Colocation of data and policy   | Couples validation policy to interchange format                              | Rejected; data/policy separation is explicit design intent         |
| Global `~/.khive/kg/rules.toml`             | One file for all projects       | Projects have different ontologies; team rules should travel with the repo   | Rejected; per-project file is git-tracked and PR-reviewable        |
| Automatic fix on validation                 | Zero extra command              | Non-idempotent; silent file modification in hooks                            | Rejected; `--fix` is explicit and deliberate                       |

## Consequences

### Positive

- Teams enforce domain-specific invariants (required properties, naming, edge density)
  without touching khive core.
- Violations surface at commit time via the pre-commit hook, before a PR is opened.
- CI produces inline PR diff annotations via `--format github`.
- `--fix` reduces friction for mechanical violations (sort order, naming normalization).
- The Rust API gives pack authors a typed validator declaration surface for the deferred CLI
  integration.
- `rules.toml` schema validation with error messages surfaces misconfiguration immediately
  with exit code 2, distinct from KG violations (exit code 1).

### Negative

- Pack-provided Rust validators are not yet executed by the CLI. Contributors use the TOML
  RulePass until that integration is implemented; scripted runtimes remain out of scope.
- `rules.toml` is a new file teams must maintain. Projects that only need ADR-020 built-in
  checks can omit it entirely; the maintenance cost is zero unless opted in.
- `--fix` for `naming-convention` modifies entity names. If entity names are referenced in
  external documentation or cross-repo edges in other repositories, renaming them creates
  a consistency gap. Contributors should review fix output before committing.

### Neutral

- The ADR-020 structural pass is unchanged. Existing `kkernel kg validate` invocations
  continue to work and gain the new rules transparently.
- The JSON output format (`--format json`) extends the ADR-020 exit-code contract: 0 for
  clean, 1 for violations, 2 for rule-file parse or unsupported-format errors. The text format is a superset
  of the ADR-020 single-line-per-check output.
- `kkernel kg init` remains backward-compatible. Existing `.khive/kg/` directories
  are unaffected; current init does not generate a rules file.

## Open Questions

1. **Non-Rust rule runtimes**: When a downstream consumer presents a concrete requirement
   for Deno/TypeScript or WASM executable rules, a follow-up ADR should define: module
   format contract, permission model, sandbox story, packaging path, and failure modes.
   A future ADR should define any `module` key or executable-rule configuration before it is
   accepted in `rules.toml`.

2. **`rules.toml` inheritance**: Should projects be able to extend a shared `rules.toml`
   (e.g., from an organization's pack) rather than declaring all rules from scratch? An
   `extends:` key at the top level is the natural shape; deferred pending demand.

## References

- [ADR-001](./ADR-001-entity-kind-taxonomy.md): Entity kind taxonomy: entity kinds
  referenced in `required-properties` config entries
- [ADR-002](./ADR-002-edge-ontology.md): Edge ontology: closed edge relation set validated
  by structural and custom rules
- [ADR-013](./ADR-013-note-kind-taxonomy.md): Note kind taxonomy: note kinds in property
  configuration contexts
- [ADR-020](./ADR-020-git-native-kg-implementation.md): Git-native KG implementation:
  defines `kkernel kg validate` built-in structural checks that this ADR's RulePass extends;
  `kkernel kg init` and `kkernel kg hook` commands; CI workflow generation
- [ADR-023](./ADR-023-declarative-pack-format.md): pack handler and extension surface
  used by the shipped Rust validator API
- ESLint configuration reference: <https://eslint.org/docs/latest/use/configure/>
- ruff configuration reference: <https://docs.astral.sh/ruff/configuration/>
