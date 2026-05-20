# ADR-056: KG Validation Pipelines

**Status**: proposed\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-048 introduced `khive kg validate` with six built-in checks: schema compliance, referential
integrity, remote resolution, no duplicate UUIDs, sort order, and cross-repo reference format.
These built-ins guard the structural invariants that git-native versioning depends on.

Teams building domain-specific KGs need validation rules beyond these structural checks. A biology
team requires all concept entities to carry a `taxa_rank` property. A research team wants every
entity to have at least three edges before it leaves the contributor's branch. An organization
enforcing the naming conventions in ADR-001 wants automated enforcement, not manual review
comments. None of these belong in the khive core.

The analogy to linting in code is exact: ESLint and ruff are not part of the JavaScript or Python
runtimes, but every serious project uses them. They work because rules are configurable, custom
rules are first-class, and they integrate into the normal commit and CI workflows.

This ADR extends `khive kg validate` to support:

1. A declarative rule configuration format in `.khive/kg/rules.yaml`.
2. A custom rule API: TypeScript/Deno functions invoked during the validation pass.
3. Pre-commit hook integration so validation runs before a commit can be created.
4. CI/CD integration with machine-readable output and a ready-made GitHub Action.
5. A structured, human-readable report format and auto-fix support for fixable rules.
6. A mechanism for packs (ADR-050) to ship their own validation rules.

### What changes and what does not

- ADR-048 `khive kg validate` built-in checks: unchanged. This ADR adds a second pass that runs
  after the built-in pass and is governed by `.khive/kg/rules.yaml`.
- ADR-050 declarative pack format: extended. Packs may now include a `rules/` directory with
  pack-provided validation rules (see §7).
- ADR-048 `khive kg init`: extended to generate `.khive/kg/rules.yaml` and optionally install the
  pre-commit hook (see §3).

## Decision

### 1. Rule configuration

Rules are declared in `.khive/kg/rules.yaml`. The file is optional; if absent, only ADR-048
built-in checks run. When present, the built-in checks are also configurable.

```yaml
# .khive/kg/rules.yaml

rules:
  # ── Built-in rules (from ADR-048 validate) ────────────────────────────────
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
    enabled: true        # set false to skip network calls in offline environments
    resolve_remotes: false  # matches ADR-048 --resolve-remotes default

  # ── Structural rules ───────────────────────────────────────────────────────
  no-orphan-entities:
    severity: warning
    config:
      min_edges: 1       # every entity must have at least this many edges

  no-self-loops:
    severity: error

  # ── Density rules ──────────────────────────────────────────────────────────
  min-edge-density:
    severity: warning
    config:
      min_edges_per_entity: 3
      exclude_kinds: [person]   # entity kinds exempt from the density check

  # ── Property rules ─────────────────────────────────────────────────────────
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

  # ── Naming rules ───────────────────────────────────────────────────────────
  naming-convention:
    severity: warning
    config:
      entity_names: title-case   # "Flash Attention" not "flash attention"
      kind_names: lowercase      # "concept" not "Concept"

  # ── Graph size ─────────────────────────────────────────────────────────────
  max-entity-count:
    severity: info
    config:
      max: 10000
      message: "Consider splitting into multiple KGs"
```

Every rule entry has:

- **id** (the YAML key): stable identifier used in reports and fix invocations.
- **severity**: `error` | `warning` | `info`. Errors cause `khive kg validate` to exit non-zero.
  Warnings and info are printed but do not change the exit code unless `--strict` is passed.
- **enabled**: `true` (default) | `false`. A disabled rule is not evaluated and produces no output.
- **config** (optional): rule-specific parameters. Unknown config keys are treated as an error in
  `rules.yaml` itself (not a validation violation), so typos surface immediately.

Rules not listed in `rules.yaml` use their built-in defaults. This means a `rules.yaml` that
lists only `required-properties` still runs all other built-in and default rules.

### 2. Custom rule API

Custom rules are TypeScript modules that live in `.khive/kg/rules/`. They are invoked by the
Deno runtime embedded in the `khive` CLI (same runtime used by ADR-050 packs).

Each custom rule module exports a `validate` function with this signature:

```typescript
// .khive/kg/rules/no-dangling-citations.ts

export interface Entity {
  id: string;
  kind: string;
  name: string;
  description: string | null;
  properties: Record<string, unknown>;
  tags: string[];
}

export interface Edge {
  edge_id: string;
  source: string;
  target: string;   // local UUID or "<remote>:<uuid>"
  relation: string;
  weight: number;
}

export interface Schema {
  version: string;
  entity_kinds: string[];
  edge_relations: Array<{ relation: string; category: string }>;
  properties: Record<string, Array<{ key: string; values?: string[] }>>;
  remotes: Array<{ name: string; repo: string; path: string; commit: string }>;
}

export interface Violation {
  entity_id: string | null;  // null for graph-level violations
  edge_id?: string | null;
  rule_id: string;
  severity: "error" | "warning" | "info";
  message: string;
  fixable?: boolean;  // true if --fix can correct this violation
}

export function validate(
  entities: Entity[],
  edges: Edge[],
  schema: Schema,
): Violation[] {
  const errors: Violation[] = [];
  const entityIds = new Set(entities.map((e) => e.id));

  for (const edge of edges) {
    if (
      edge.relation === "cites" &&
      !entityIds.has(edge.target) &&
      !edge.target.includes(":")
    ) {
      errors.push({
        entity_id: edge.source,
        edge_id: edge.edge_id,
        rule_id: "no-dangling-citations",
        severity: "error",
        message: `Edge cites target ${edge.target} not found in entity set`,
        fixable: false,
      });
    }
  }

  return errors;
}
```

To enable a custom rule, add it to `rules.yaml`:

```yaml
rules:
  no-dangling-citations:
    severity: error
    enabled: true
    module: rules/no-dangling-citations.ts   # path relative to .khive/kg/
```

The `module` key is what distinguishes a custom rule from a built-in rule. Built-in rules have
no `module` key. The CLI loads and executes the module in a Deno sandbox with read-only access
to the `.khive/kg/` directory. No network access, no filesystem writes.

Custom rules also receive a `config` object when one is declared in `rules.yaml`. The module
receives it as a fourth argument:

```typescript
export function validate(
  entities: Entity[],
  edges: Edge[],
  schema: Schema,
  config: Record<string, unknown>,
): Violation[] { ... }
```

### 3. Git hook integration

`khive kg init` generates `.khive/kg/hooks/pre-commit` and asks whether to install it:

```
khive kg init
  Initialized .khive/kg/ (schema.yaml, entities.ndjson, edges.ndjson)
  Install pre-commit hook? [y/N]: y
  Installed: .git/hooks/pre-commit → .khive/kg/hooks/pre-commit
```

The hook script lives at `.khive/kg/hooks/pre-commit` so it is tracked by git alongside the KG
and rules. The `.git/hooks/pre-commit` entry is a symlink to the tracked script:

```bash
#!/usr/bin/env bash
# .khive/kg/hooks/pre-commit
# Generated by `khive kg init`. Runs KG validation on staged NDJSON files.
# Bypass with `git commit --no-verify` (same as any git hook).

set -euo pipefail

# Only run if KG files are staged
staged=$(git diff --cached --name-only | grep -E '^\.khive/kg/(entities|edges)\.ndjson$' || true)
if [ -z "$staged" ]; then
  exit 0
fi

khive kg validate
```

The hook:

- Runs only when `entities.ndjson` or `edges.ndjson` are staged (no false positives for unrelated
  commits).
- Exits 0 (allow commit) if validation passes without errors.
- Exits non-zero (block commit) if any rule at severity `error` is violated.
- Warnings and info are printed but do not block the commit.
- `git commit --no-verify` bypasses the hook, consistent with git conventions.

Installing the hook on an existing repo (without `init`) is also supported:

```
khive kg hook install    # installs symlink
khive kg hook uninstall  # removes symlink, leaves .khive/kg/hooks/pre-commit
khive kg hook status     # shows whether hook is installed and if symlink is valid
```

### 4. CI/CD integration

#### GitHub Action

A GitHub Action `khive/kg-validate-action@v1` wraps `khive kg validate --ci` for use in
workflows:

```yaml
# .github/workflows/kg-validate.yml
# Generated by `khive kg init --ci` (extends ADR-048 §6 workflow generation)
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
      - uses: cargo-bins/cargo-binstall@main
      - run: cargo binstall khive-cli --no-confirm
      - name: Validate KG
        uses: khive/kg-validate-action@v1
        with:
          rules: .khive/kg/rules.yaml    # default; override to point elsewhere
          fail-on: error                 # "error" | "warning" | "never"
          format: github                 # "github" | "json" | "text"
          resolve-remotes: "true"        # enables full cross-repo resolution in CI
```

The `format: github` output mode uses GitHub Actions annotations
(`::error file=...::` / `::warning file=...::`) to surface violations inline in the PR diff view.

#### Machine-readable output

`khive kg validate --format json` produces a single JSON object on stdout, suitable for any CI
system:

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

`passed` at the top level is `true` when `errors == 0`. The exit code mirrors this: 0 when
`passed == true`, 1 otherwise. With `--strict`, the exit code is non-zero if `warnings > 0`.

#### Output for human terminals

`khive kg validate` (no flags) uses the text report format:

```
khive kg validate
  ✓ schema-compliance (420 entities, 1100 edges)
  ✓ referential-integrity
  ✓ no-duplicate-uuids
  ⚠ sort-order: 0 violations
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

Symbols: `✓` = passed, `⚠` = warning (printed but exit 0), `✗` = error (exit 1).

`--verbose` expands all violation lists. `--quiet` suppresses per-rule lines and shows only the
summary line.

### 5. Auto-fix support

Rules that can be automatically corrected declare `fixable: true` in their violation records.
`khive kg validate --fix` applies all fixable rules and reports what changed:

```
khive kg validate --fix
  ✓ schema-compliance
  ~ sort-order: re-sorted entities.ndjson (3 lines moved)
  ~ naming-convention: normalized 5 entity names to title-case
    - "flash attention" → "Flash Attention" (concept, id: 4b2a1c3d)
    - "lora" → "LoRA" (concept, id: c3f1a2b4)
    + 3 more
  ✗ required-properties: 5 entities missing required properties (cannot auto-fix)

Summary: 1 error fixed, 1 error unfixable, 420 entities, 1100 edges
```

`--fix` writes to `entities.ndjson` and `edges.ndjson` in place. Files are only written if at
least one fixable violation was found; no spurious writes occur. The pre-commit hook does not run
`--fix` automatically; the contributor must run it explicitly.

Built-in fixable rules:

| Rule | Fix behavior |
|------|-------------|
| `sort-order` | Re-sorts both NDJSON files in the canonical sort order |
| `naming-convention` (entity_names) | Normalizes entity names to title-case per config |

Built-in unfixable rules (require human judgment):

| Rule | Why unfixable |
|------|--------------|
| `required-properties` | The value must come from the contributor |
| `min-edge-density` | Which edges to add is a semantic decision |
| `no-orphan-entities` | Whether to add edges or delete the entity is context-dependent |

Custom rules may declare their violations fixable and provide a `fix` export alongside `validate`:

```typescript
export function fix(
  entities: Entity[],
  edges: Edge[],
  violations: Violation[],
  config: Record<string, unknown>,
): { entities: Entity[]; edges: Edge[] } {
  // Return the corrected entity and edge arrays.
  // The caller writes them back to NDJSON.
}
```

### 6. Rule evaluation order

The validation pass runs in a defined order:

1. **ADR-048 built-in structural checks** (schema compliance, referential integrity, duplicate
   UUIDs, sort order, remote resolution). These run first and fast, before any rule file is
   loaded.
2. **Built-in configurable rules** declared in `rules.yaml` without a `module` key (orphan
   entities, self-loops, edge density, required properties, naming convention, max entity count).
3. **Custom rules** declared in `rules.yaml` with a `module` key, in the order they appear in the
   file.
4. **Pack-provided rules** (see §7), sorted by pack installation order.

If a built-in structural check (step 1) produces an error, the remaining passes still run and
their violations are included in the report. This gives contributors a complete picture in a
single run rather than requiring iterative fix-and-validate cycles.

### 7. Pack-provided rules

A pack (ADR-050) may include a `validation/` directory at its pack root. Each file in that
directory is a TypeScript rule module with the same `validate` / optional `fix` signature as
custom rules (§2).

The pack declares which rules it provides in its `pack.yaml`:

```yaml
# In a pack's pack.yaml
validation:
  - id: biology/required-taxa-rank
    severity: warning
    module: validation/required-taxa-rank.ts
    description: "Requires all concept entities to carry a taxa_rank property"
```

When a pack is installed, its validation rules are merged into the validation pipeline. Pack
rule IDs are namespaced by pack name to avoid collisions: `<pack-name>/<rule-id>`. Projects can
override a pack rule's severity or disable it in their `rules.yaml`:

```yaml
rules:
  biology/required-taxa-rank:
    severity: error    # escalate from pack default of warning
    enabled: true
```

Pack rules that are not mentioned in `rules.yaml` run with the severity declared in the pack's
`pack.yaml`.

### 8. `rules.yaml` schema validation

`khive kg validate` validates `rules.yaml` itself against a built-in JSON Schema before
evaluating any rules. A malformed `rules.yaml` (unknown top-level key, invalid severity value,
`module` pointing to a non-existent file, unknown `config` key for a built-in rule) produces a
structured error that names the offending field and aborts before touching the NDJSON files:

```
ERROR: rules.yaml line 14: unknown config key "min_edges_per_node" for rule "min-edge-density"
  Did you mean "min_edges_per_entity"?
```

This schema validation is separate from and prior to KG validation. The exit code for a
`rules.yaml` parse error is 2 (distinct from 1 for KG violations, 0 for pass). CI pipelines
can distinguish the two failure modes.

## Rationale

### Why `.khive/kg/rules.yaml` rather than inline schema annotations

Keeping rule configuration in a dedicated file separates policy from data. The NDJSON entity and
edge files should remain pure data — adding inline rule annotations would couple the validation
system to the data format and break ADR-048's goal of a clean interchange format that any tool
can consume. `rules.yaml` is the policy layer; NDJSON is the data layer.

### Why TypeScript/Deno for custom rules rather than WASM or Lua

The Deno runtime is already embedded in the `khive` CLI for ADR-050 declarative packs. Using the
same runtime for custom validation rules avoids a second plugin execution model. TypeScript gives
contributors a typed, familiar language with good IDE support. WASM is the right choice for
performance-critical custom rules if that need emerges; WASM can be added as a second module
format without changing the rule API.

Lua was considered for its minimal footprint but rejected because TypeScript types give
contributors early feedback when their rule's return shape is wrong, and because the Deno sandbox
model is already established.

### Why severity is per-rule rather than per-violation

Per-rule severity is the model used by ESLint, ruff, and every mature linter. A rule produces
violations of one severity — the tool author decides whether a violation class is an error or a
warning. Per-violation severity is supported only in the custom rule API (where the rule author
controls the `Violation` objects) and is how pack rules can emit mixed-severity output within a
single rule. For built-in rules and YAML-configured rules, severity is a single scalar because
that is what projects want to configure.

### Why auto-fix is explicit (`--fix`) rather than automatic

Automatic fixes during validation create a non-idempotent operation. A validation run that also
modifies files means the output state is not predictable from the input state alone. Pre-commit
hooks should be read-only by default: they report what is wrong; the contributor decides whether
to apply the fix. `--fix` is explicit and produces a diff report so contributors see exactly
what changed.

### Why pack-provided rules use a namespace prefix

Without a namespace prefix, a pack named `biology` and a project's custom rule could both declare
a rule named `required-taxa-rank`. The collision would be silent in `rules.yaml` and produce
confusing behavior. The `<pack-name>/` prefix eliminates the ambiguity and makes the rule's
provenance visible in violation reports.

### Why exit code 2 for `rules.yaml` errors

CI pipelines benefit from distinguishing between "the KG has validation violations" (exit 1) and
"the rules file itself is malformed" (exit 2). The first means contributors need to fix their KG
changes. The second means an infrastructure maintainer needs to fix the rules file. Separate exit
codes allow pipeline steps to route these cases to different notifications or reviewers.

## Alternatives Considered

| Alternative | Pros | Cons | Why rejected |
|---|---|---|---|
| No custom rules, only built-in | Simple, zero config | Teams have domain constraints that built-ins cannot express | Rejected: the biology example alone shows built-ins are insufficient |
| JSON Schema for entity property validation | Widely known, good tooling | Cannot express structural rules (edge density, orphan detection, graph topology) | Partial — JSON Schema is incorporated implicitly via required-properties rule, but is insufficient as the sole validation mechanism |
| WASM plugins for custom rules | Performance, language-agnostic | Complex build chain for contributors; no shared Deno runtime benefit | Deferred: add as a second module format if TypeScript performance is insufficient |
| Server-side validation only (cloud API call) | No local tooling required | Breaks local-first, offline, and CI reproducibility guarantees from ADR-048 | Rejected: local-first is a hard requirement |
| Per-entity inline rule annotations (NDJSON field) | Colocation of data and policy | Couples validation policy to data format; breaks interchange | Rejected: separating policy and data layers is explicit design intent |
| Global `~/.khive/kg/rules.yaml` rather than per-project | One file for all projects | Projects have different ontologies and constraints; team-level rules should travel with the repo | Rejected: per-project `rules.yaml` is git-tracked and reviewable in PRs |

## Consequences

### Positive

- Teams can enforce domain-specific invariants (required properties, naming conventions, edge
  density) without modifying khive core.
- Validation failures are caught at commit time via the pre-commit hook, before a PR is opened.
- CI reports violations inline in the PR diff via GitHub Actions annotations.
- `--fix` reduces contributor friction for mechanical violations (sort order, naming).
- Pack authors can bundle rules alongside their vocabulary, keeping domain constraints colocated
  with the pack they govern.
- The `rules.yaml` schema validation with helpful error messages means misconfiguration surfaces
  immediately rather than producing silent wrong behavior.

### Negative

- Contributors must have the Deno runtime available for custom rule execution. The Deno binary
  is bundled with the khive CLI on the supported platforms but adds to the binary size.
- Custom rules introduce a code execution surface. The Deno sandbox (read-only filesystem,
  no network) mitigates but does not eliminate risk for rules from untrusted sources. Rule
  provenance (project rules vs. pack rules) is shown in reports.
- `rules.yaml` is a new configuration file that projects must maintain. For projects that only
  need ADR-048 built-in checks, this file is optional, so the maintenance burden is zero unless
  opted in.
- Auto-fix for `naming-convention` modifies entity names. If entity names are referenced in
  external documentation or other repos' cross-repo edges, renaming them creates a consistency
  gap until those references are updated. Contributors should review fix output before committing.

### Neutral

- The ADR-048 `khive kg validate` built-in pass is unchanged. This ADR adds a second pass that
  runs after the built-ins. Existing CI workflows that call `khive kg validate` continue to work
  and gain the new rules without any migration.
- The JSON output format (`--format json`) extends ADR-048's `validate` exit-code contract:
  0 for clean, 1 for errors, 2 for `rules.yaml` parse errors. The text output format is a
  superset of ADR-048's single-line-per-check format.
- The `khive kg init` command is extended but backward-compatible. Existing `.khive/kg/`
  directories are not affected; `init --add-hooks` installs the hook without re-initializing.

## Implementation

### CLI extensions

`crates/khive-vcs/src/validate.rs` gains a second `RulePass` that runs after the built-in
`StructuralPass`. The `RulePass`:

1. Reads and parses `.khive/kg/rules.yaml` (absent = no-op).
2. Loads built-in configurable rules keyed by the rule IDs in §1.
3. Loads custom rule modules from `module:` paths using the embedded Deno runtime.
4. Loads pack-provided rules from all installed packs.
5. Runs each enabled rule against the in-memory parsed NDJSON content.
6. Merges violations into the structured `ValidationReport` already produced by the structural pass.

```
crates/khive-vcs/src/
  validate.rs          — extended: StructuralPass + RulePass merged into ValidationReport
  rules/
    loader.rs          — rules.yaml parse + schema validation
    builtin.rs         — built-in configurable rules (density, orphans, naming, etc.)
    custom.rs          — Deno module invocation sandbox
    pack.rs            — pack-provided rule loading
  fix.rs               — --fix support: applies fixable violations, writes NDJSON
  report.rs            — ValidationReport → text / json / github-actions formatter
```

New CLI subcommands added to `crates/khive-cli/src/kg.rs`:

| Subcommand | Behavior |
|---|---|
| `khive kg hook install` | Installs pre-commit hook symlink |
| `khive kg hook uninstall` | Removes symlink; leaves hook script |
| `khive kg hook status` | Prints whether hook is installed and symlink is valid |

`khive kg validate` gains flags:

| Flag | Behavior |
|---|---|
| `--fix` | Apply fixable rules and report changes |
| `--strict` | Treat warnings as errors (non-zero exit) |
| `--format text\|json\|github` | Output format (default: text) |
| `--verbose` | Expand all violation lists |
| `--quiet` | Show summary line only |
| `--rules <path>` | Override default rules.yaml path |
| `--no-rules` | Run built-in structural checks only, skip rules.yaml |

### Phasing

| Phase | Scope | Target |
|---|---|---|
| 1 | `rules.yaml` loader + schema validation + built-in configurable rules (density, orphans, self-loops, required-properties, naming, max-count) | v0.5 |
| 2 | `--format json` + text report upgrade + `--fix` for sort-order and naming-convention | v0.5 |
| 3 | Deno custom rule API + sandbox + `module:` loading | v0.5 |
| 4 | `khive kg hook install/uninstall/status` + pre-commit hook generation in `init` | v0.5 |
| 5 | `khive/kg-validate-action@v1` GitHub Action + `--format github` | v0.6 |
| 6 | Pack-provided rules (`validation/` directory in pack) | v0.6 |

Phases 1 and 2 deliver the highest immediate value (configurable built-in rules + CI-readable
output) and are independently shippable. Phase 3 (custom rules) requires the Deno sandbox.
Phase 5 (GitHub Action) is a separate repository and can be published independently of the CLI.

## References

- ADR-048: Git-Native KG Versioning (defines `khive kg validate` built-in checks; this ADR
  extends the validation pass)
- ADR-050: Declarative Pack Format (pack structure that this ADR extends with `validation/` dir)
- ADR-051: CLI Auth and KG Git Workflow (CLI integration context)
- ADR-002: Closed Edge Ontology (edge relations validated by rules in this ADR)
- ADR-001: Entity Kind Taxonomy (entity kinds referenced in required-properties config)
- [CLAUDE.md projects/CLAUDE.md](../../../../CLAUDE.md) — edge density minimums (≥4 edges/entity
  average; concept ≥4 edges) that the `min-edge-density` rule enforces
- ESLint configuration reference: https://eslint.org/docs/latest/use/configure/
- ruff configuration reference: https://docs.astral.sh/ruff/configuration/
- Deno sandboxing documentation: https://docs.deno.com/runtime/fundamentals/security/
