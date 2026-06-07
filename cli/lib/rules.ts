/**
 * Rule-based validation pass (ADR-056 §1).
 *
 * Reads .khive/kg/rules.yaml (optional). Each rule is keyed by its stable ID
 * (e.g. "no-self-loops", "required-properties") and carries a severity
 * (`error|warning|info`), an `enabled` flag, and optional config.
 *
 * Phase E1 rules (built-in only):
 *   - no-self-loops         edges where source == target
 *   - no-orphan-entities    entities with < min_edges edges
 *   - required-properties   per-kind required property keys
 *   - max-entity-count      cap on number of entities
 *   - min-edge-density      avg edges per entity below threshold
 *
 * Custom rules (TypeScript modules under .khive/kg/rules/) and pack-provided
 * rules are Phase E2 (need sandbox loader + ADR-050 pack runtime).
 */

import { parse as parseYaml } from "@std/yaml";
import { join } from "@std/path";
import { ENTITY_KINDS, parseEdgeLine, parseEntityLine, readNdjson } from "./ndjson.ts";
import { EDGES_FILE, ENTITIES_FILE } from "./paths.ts";

// ─── Types ────────────────────────────────────────────────────────────────────

export type Severity = "error" | "warning" | "info";

export interface RuleViolation {
  /** Rule ID, e.g. "no-self-loops". */
  rule: string;
  severity: Severity;
  /** Per-line file location when applicable; 0 for graph-wide. */
  file?: string;
  line?: number;
  /** Stable subject id for the violation (entity UUID, edge composite key, etc.) */
  subject?: string;
  message: string;
}

export interface RuleConfigEntry {
  severity?: Severity;
  enabled?: boolean;
  config?: Record<string, unknown>;
}

export interface RulesFile {
  rules?: Record<string, RuleConfigEntry>;
}

/**
 * A structured error produced when rules.yaml itself is malformed.
 * These are distinct from KG violations: exit code 2 rather than 1.
 */
export interface RulesFileError {
  message: string;
}

// ─── Built-in rule registry ───────────────────────────────────────────────────

const BUILTIN_RULES = new Set([
  "no-self-loops",
  "no-orphan-entities",
  "required-properties",
  "max-entity-count",
  "min-edge-density",
]);

// Severity defaults if rules.yaml does not override.
const DEFAULT_SEVERITY: Record<string, Severity> = {
  "no-self-loops": "error",
  "no-orphan-entities": "warning",
  "required-properties": "error",
  "max-entity-count": "info",
  "min-edge-density": "warning",
};

// Valid config keys per built-in rule (ADR-056 §8 closed-key validation).
const VALID_BUILTIN_CONFIG_KEYS: Record<string, Set<string>> = {
  "no-self-loops": new Set(),
  "no-orphan-entities": new Set(["min_edges"]),
  "required-properties": new Set([...ENTITY_KINDS]),
  "max-entity-count": new Set(["max", "message"]),
  "min-edge-density": new Set(["min_edges_per_entity", "exclude_kinds"]),
};

const VALID_SEVERITIES = new Set<string>(["error", "warning", "info"]);

// ─── rules.yaml schema validation ────────────────────────────────────────────

/**
 * Validate rules.yaml structure before any rule evaluation.
 *
 * Returns a (possibly empty) list of structured errors. A non-empty list
 * means the caller should exit with code 2 without running any rules.
 */
export function validateRulesFile(parsed: unknown): RulesFileError[] {
  const errors: RulesFileError[] = [];

  if (parsed === null || parsed === undefined) {
    // Empty file is valid.
    return errors;
  }

  if (typeof parsed !== "object" || Array.isArray(parsed)) {
    errors.push({ message: "rules.yaml must be a YAML mapping, not a scalar or array" });
    return errors;
  }

  const top = parsed as Record<string, unknown>;

  // Check for unknown top-level keys.
  const VALID_TOP_KEYS = new Set(["rules"]);
  for (const key of Object.keys(top)) {
    if (!VALID_TOP_KEYS.has(key)) {
      errors.push({ message: `rules.yaml: unknown top-level key '${key}'` });
    }
  }

  if (top["rules"] === undefined || top["rules"] === null) {
    return errors;
  }

  if (typeof top["rules"] !== "object" || Array.isArray(top["rules"])) {
    errors.push({ message: "rules.yaml: 'rules' must be a mapping of rule-id to rule config" });
    return errors;
  }

  const rules = top["rules"] as Record<string, unknown>;

  for (const [ruleId, entry] of Object.entries(rules)) {
    const prefix = `rules.yaml: rule '${ruleId}'`;

    if (entry === null || entry === undefined) {
      // Null entry means "use defaults" — acceptable.
      continue;
    }

    if (typeof entry !== "object" || Array.isArray(entry)) {
      errors.push({ message: `${prefix}: entry must be a mapping, not a scalar or array` });
      continue;
    }

    const cfg = entry as Record<string, unknown>;

    // Validate unknown entry-level keys.
    const VALID_ENTRY_KEYS = new Set(["severity", "enabled", "config"]);
    for (const key of Object.keys(cfg)) {
      if (key === "module") {
        errors.push({
          message:
            `${prefix}: 'module' (custom rule modules) is a Phase E2 feature and is not yet supported`,
        });
        continue;
      }
      if (!VALID_ENTRY_KEYS.has(key)) {
        errors.push({ message: `${prefix}: unknown entry key '${key}'` });
      }
    }

    // Validate severity value.
    if (cfg["severity"] !== undefined) {
      if (!VALID_SEVERITIES.has(String(cfg["severity"]))) {
        errors.push({
          message: `${prefix}: invalid severity '${cfg["severity"]}' (must be error|warning|info)`,
        });
      }
    }

    // Validate enabled type.
    if (cfg["enabled"] !== undefined && typeof cfg["enabled"] !== "boolean") {
      errors.push({
        message: `${prefix}: 'enabled' must be a boolean, got ${typeof cfg["enabled"]}`,
      });
    }

    // Validate config keys for known built-in rules.
    if (BUILTIN_RULES.has(ruleId) && cfg["config"] !== undefined) {
      if (typeof cfg["config"] !== "object" || Array.isArray(cfg["config"])) {
        errors.push({ message: `${prefix}: 'config' must be a mapping` });
        continue;
      }

      const configMap = cfg["config"] as Record<string, unknown>;
      const validKeys = VALID_BUILTIN_CONFIG_KEYS[ruleId];

      for (const configKey of Object.keys(configMap)) {
        if (!validKeys.has(configKey)) {
          if (ruleId === "required-properties") {
            // Config keys for required-properties are entity kinds — give a
            // more informative message than "unknown config key".
            errors.push({
              message: `${prefix}: config key '${configKey}' is not a valid entity kind (ADR-001: ${
                ENTITY_KINDS.join(", ")
              })`,
            });
          } else {
            const allValid = [...validKeys].join(", ");
            errors.push({
              message: `${prefix}: unknown config key '${configKey}' (valid keys: ${
                allValid || "none"
              })`,
            });
          }
        }
      }

      // For min-edge-density, validate exclude_kinds values.
      if (ruleId === "min-edge-density" && "exclude_kinds" in configMap) {
        const excludeKinds = configMap["exclude_kinds"];
        if (!Array.isArray(excludeKinds)) {
          errors.push({ message: `${prefix}: config 'exclude_kinds' must be an array` });
        } else {
          const entityKindSet = new Set<string>(ENTITY_KINDS);
          for (const kind of excludeKinds) {
            if (typeof kind !== "string" || !entityKindSet.has(kind)) {
              errors.push({
                message:
                  `${prefix}: config 'exclude_kinds' entry '${kind}' is not a valid entity kind (ADR-001: ${
                    ENTITY_KINDS.join(", ")
                  })`,
              });
            }
          }
        }
      }
    }
  }

  return errors;
}

// ─── Loader ───────────────────────────────────────────────────────────────────

export interface ResolvedRule {
  id: string;
  severity: Severity;
  enabled: boolean;
  config: Record<string, unknown>;
}

/**
 * Load and validate rules.yaml, then resolve the full rule list.
 *
 * Throws `RulesFileErrors` (an array) when schema validation fails so the
 * caller can distinguish rules-file errors (exit 2) from KG violations (exit 1).
 */
export class RulesFileErrors extends Error {
  constructor(public readonly errors: RulesFileError[]) {
    super(errors.map((e) => e.message).join("; "));
    this.name = "RulesFileErrors";
  }
}

export async function loadRules(repoRoot: string): Promise<ResolvedRule[]> {
  const path = join(repoRoot, ".khive/kg/rules.yaml");
  let rawParsed: unknown = null;
  try {
    const text = await Deno.readTextFile(path);
    rawParsed = parseYaml(text) ?? null;
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) throw err;
    // No rules.yaml — use defaults for built-ins.
    rawParsed = null;
  }

  // Schema validation before trusting any values (ADR-056 §8).
  const schemaErrors = validateRulesFile(rawParsed);
  if (schemaErrors.length > 0) {
    throw new RulesFileErrors(schemaErrors);
  }

  const parsed: RulesFile = (rawParsed as RulesFile | null) ?? {};

  const out: ResolvedRule[] = [];
  // Always evaluate built-ins (rules.yaml only customizes them).
  for (const id of BUILTIN_RULES) {
    const entry = parsed.rules?.[id] ?? {};
    out.push({
      id,
      severity: (entry.severity as Severity | undefined) ?? DEFAULT_SEVERITY[id],
      // Default disabled for max-entity-count and min-edge-density unless
      // rules.yaml explicitly enables them with config (they're noisy in tiny
      // graphs).
      enabled: entry.enabled ?? (
        id === "max-entity-count" || id === "min-edge-density"
          ? Boolean(entry.config)
          : id === "required-properties"
          ? Boolean(entry.config)
          : true
      ),
      config: entry.config ?? {},
    });
  }

  // Surface any unknown rule IDs in rules.yaml as warnings (so typos surface).
  if (parsed.rules) {
    for (const id of Object.keys(parsed.rules)) {
      if (!BUILTIN_RULES.has(id)) {
        out.push({
          id: "_unknown_rule",
          severity: "warning",
          enabled: true,
          config: { reportedId: id },
        });
      }
    }
  }
  return out;
}

// ─── Loaded NDJSON ───────────────────────────────────────────────────────────

export interface LoadedGraph {
  entitiesByKind: Map<string, Set<string>>;
  entityKindOf: Map<string, string>;
  entityProps: Map<string, Record<string, unknown>>;
  edges: Array<{ source: string; target: string; relation: string }>;
}

async function loadGraph(repoRoot: string): Promise<LoadedGraph> {
  const entitiesByKind = new Map<string, Set<string>>();
  const entityKindOf = new Map<string, string>();
  const entityProps = new Map<string, Record<string, unknown>>();
  const edges: Array<{ source: string; target: string; relation: string }> = [];

  for await (const entry of readNdjson(`${repoRoot}/${ENTITIES_FILE}`)) {
    if (entry.data === null) continue;
    const e = parseEntityLine(entry.data);
    if (!e) continue;
    if (!entitiesByKind.has(e.kind)) entitiesByKind.set(e.kind, new Set());
    entitiesByKind.get(e.kind)!.add(e.id);
    entityKindOf.set(e.id, e.kind);
    const props = (entry.data as Record<string, unknown>).properties;
    if (props && typeof props === "object" && !Array.isArray(props)) {
      entityProps.set(e.id, props as Record<string, unknown>);
    } else {
      entityProps.set(e.id, {});
    }
  }

  for await (const entry of readNdjson(`${repoRoot}/${EDGES_FILE}`)) {
    if (entry.data === null) continue;
    const e = parseEdgeLine(entry.data);
    if (!e) continue;
    edges.push({ source: e.source, target: e.target, relation: e.relation });
  }

  return { entitiesByKind, entityKindOf, entityProps, edges };
}

// ─── Rule implementations ────────────────────────────────────────────────────

function runNoSelfLoops(rule: ResolvedRule, g: LoadedGraph): RuleViolation[] {
  const out: RuleViolation[] = [];
  for (const e of g.edges) {
    if (e.source === e.target) {
      out.push({
        rule: rule.id,
        severity: rule.severity,
        file: EDGES_FILE,
        subject: `${e.source}:${e.target}:${e.relation}`,
        message: `self-loop edge: ${e.source} -[${e.relation}]-> itself`,
      });
    }
  }
  return out;
}

function runNoOrphans(rule: ResolvedRule, g: LoadedGraph): RuleViolation[] {
  const minEdges = (rule.config["min_edges"] as number | undefined) ?? 1;
  const edgeCount = new Map<string, number>();
  for (const e of g.edges) {
    edgeCount.set(e.source, (edgeCount.get(e.source) ?? 0) + 1);
    edgeCount.set(e.target, (edgeCount.get(e.target) ?? 0) + 1);
  }
  const out: RuleViolation[] = [];
  for (const [id] of g.entityKindOf) {
    const c = edgeCount.get(id) ?? 0;
    if (c < minEdges) {
      out.push({
        rule: rule.id,
        severity: rule.severity,
        subject: id,
        message: `entity ${id} has ${c} edges (rule requires >= ${minEdges})`,
      });
    }
  }
  return out;
}

function runRequiredProperties(
  rule: ResolvedRule,
  g: LoadedGraph,
): RuleViolation[] {
  // config = { concept: ["description", "domain"], document: [...] }
  // Config keys are validated against ENTITY_KINDS in validateRulesFile().
  const cfg = rule.config as Record<string, string[]>;
  const out: RuleViolation[] = [];
  for (const [kind, ids] of g.entitiesByKind) {
    const required = cfg[kind];
    if (!required || required.length === 0) continue;
    for (const id of ids) {
      const props = g.entityProps.get(id) ?? {};
      for (const key of required) {
        const v = props[key];
        if (v === undefined || v === null || v === "") {
          out.push({
            rule: rule.id,
            severity: rule.severity,
            subject: id,
            message: `entity ${id} (kind=${kind}) missing required property '${key}'`,
          });
        }
      }
    }
  }
  return out;
}

function runMaxEntityCount(
  rule: ResolvedRule,
  g: LoadedGraph,
): RuleViolation[] {
  const max = (rule.config["max"] as number | undefined) ?? Infinity;
  const total = g.entityKindOf.size;
  if (total > max) {
    const msg = (rule.config["message"] as string | undefined) ??
      `entity count ${total} exceeds maximum ${max}`;
    return [{
      rule: rule.id,
      severity: rule.severity,
      message: msg,
    }];
  }
  return [];
}

function runMinEdgeDensity(
  rule: ResolvedRule,
  g: LoadedGraph,
): RuleViolation[] {
  const target = (rule.config["min_edges_per_entity"] as number | undefined) ?? 1;
  // exclude_kinds values are validated against ENTITY_KINDS in validateRulesFile().
  const excludeKinds = new Set(
    (rule.config["exclude_kinds"] as string[] | undefined) ?? [],
  );

  let entityCount = 0;
  for (const [kind, ids] of g.entitiesByKind) {
    if (excludeKinds.has(kind)) continue;
    entityCount += ids.size;
  }
  if (entityCount === 0) return [];

  // Each edge contributes 2 to "incident edges" count, but the conventional
  // density metric is edges-per-entity (single-count incidence).
  const edgeCount = g.edges.length;
  const avg = edgeCount / entityCount;
  if (avg < target) {
    return [{
      rule: rule.id,
      severity: rule.severity,
      message: `graph density ${
        avg.toFixed(2)
      } below target ${target} (${edgeCount} edges over ${entityCount} entities)`,
    }];
  }
  return [];
}

// ─── Main runner ─────────────────────────────────────────────────────────────

export interface RuleRunSummary {
  violations: RuleViolation[];
  evaluated: number;
  skippedDisabled: string[];
}

export async function runRules(repoRoot: string): Promise<RuleRunSummary> {
  const rules = await loadRules(repoRoot);
  const violations: RuleViolation[] = [];
  let evaluated = 0;
  const skippedDisabled: string[] = [];

  // Load graph once.
  const graph = await loadGraph(repoRoot);

  for (const rule of rules) {
    if (!rule.enabled) {
      skippedDisabled.push(rule.id);
      continue;
    }
    if (rule.id === "_unknown_rule") {
      violations.push({
        rule: rule.id,
        severity: rule.severity,
        message: `rules.yaml references unknown rule '${rule.config["reportedId"]}' (typo?)`,
      });
      continue;
    }

    evaluated++;
    switch (rule.id) {
      case "no-self-loops":
        violations.push(...runNoSelfLoops(rule, graph));
        break;
      case "no-orphan-entities":
        violations.push(...runNoOrphans(rule, graph));
        break;
      case "required-properties":
        violations.push(...runRequiredProperties(rule, graph));
        break;
      case "max-entity-count":
        violations.push(...runMaxEntityCount(rule, graph));
        break;
      case "min-edge-density":
        violations.push(...runMinEdgeDensity(rule, graph));
        break;
      default:
        // Unknown built-in (shouldn't happen — guarded by BUILTIN_RULES).
        break;
    }
  }

  return { violations, evaluated, skippedDisabled };
}
