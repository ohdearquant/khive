/**
 * `khive kg migrate` — apply schema migrations to NDJSON (ADR-054 §2).
 *
 * Reads YAML migrations from `.khive/kg/migrations/NNNN_description.yaml` and
 * applies them in filename order. Each migration is atomic — if any operation
 * fails the entire migration is rolled back and `ontology_version` is not
 * advanced.
 *
 * Supported operations (Phase E1):
 *   add_kind                  validates against ADR-001 base kinds or current schema
 *   remove_kind               on_existing: error → abort if entities of this kind exist
 *                             on_existing: migrate_to → rewrite kind on matching entities
 *   rename_kind               rewrite kind field across all matching entities
 *   add_property              no rewrite unless required: true and any entity lacks it;
 *                             also records property in schema entity_properties map
 *   remove_property           removes property from schema entity_properties map
 *   rename_property           rewrite properties.<from> → properties.<to>
 *                             and updates schema entity_properties map
 *   add_relation_endpoint     records endpoint rule in schema; no NDJSON rewrite
 *   remove_relation_endpoint  matches (relation, source_kind, target_kind) exactly;
 *                             on_existing: error → abort if matching edges exist;
 *                             on_existing: drop → remove matching edges
 *
 * Not yet wired (Phase E2):
 *   change_property_type with coerce — needs typed coercion helpers
 *
 * Usage:
 *   khive kg migrate                Apply all pending migrations.
 *   khive kg migrate --dry-run      Plan + print, don't write.
 *   khive kg migrate --to 1.2.0     Apply up to and including a target version.
 *   khive kg migrate --list         List pending and applied migrations.
 */

import { parse as parseYaml, stringify as stringifyYaml } from "@std/yaml";
import { join } from "@std/path";
import { EDGES_FILE, ENTITIES_FILE, MIGRATIONS_DIR, SCHEMA_FILE } from "../lib/paths.ts";

// ─── ADR-001 base entity kinds (closed taxonomy) ─────────────────────────────

const BASE_ENTITY_KINDS = new Set([
  "concept",
  "document",
  "dataset",
  "project",
  "person",
  "org",
]);

// ─── ADR-002 canonical edge relations (closed set) ───────────────────────────

const CANONICAL_RELATIONS = new Set([
  "contains",
  "part_of",
  "instance_of",
  "extends",
  "variant_of",
  "introduced_by",
  "supersedes",
  "depends_on",
  "enables",
  "implements",
  "competes_with",
  "composed_with",
  "annotates",
]);

// ─── Migration file types ────────────────────────────────────────────────────

interface AddKind {
  add_kind: { name: string; description?: string };
}
interface RemoveKind {
  remove_kind: { name: string; on_existing: "error" | "migrate_to"; target?: string };
}
interface RenameKind {
  rename_kind: { from: string; to: string };
}
interface AddProperty {
  add_property: {
    kind: string;
    name: string;
    type: string;
    required?: boolean;
    description?: string;
  };
}
interface RemoveProperty {
  remove_property: { kind: string; name: string };
}
interface RenameProperty {
  rename_property: { kind: string; from: string; to: string };
}
interface AddRelationEndpoint {
  add_relation_endpoint: { relation: string; source_kind: string; target_kind: string };
}
interface RemoveRelationEndpoint {
  remove_relation_endpoint: {
    relation: string;
    source_kind: string;
    target_kind: string;
    on_existing: "error" | "drop";
  };
}

type Operation =
  | AddKind
  | RemoveKind
  | RenameKind
  | AddProperty
  | RemoveProperty
  | RenameProperty
  | AddRelationEndpoint
  | RemoveRelationEndpoint;

interface Migration {
  /** File path relative to repo root. */
  path: string;
  /** Sequence prefix from the filename (e.g. "0001"). */
  seq: number;
  version_from: string;
  version_to: string;
  description: string;
  operations: Operation[];
}

// ─── Filename parsing ────────────────────────────────────────────────────────

const FILENAME_RE = /^(\d{4})_([a-z0-9_-]+)\.ya?ml$/;

async function listMigrationFiles(repoRoot: string): Promise<Migration[]> {
  const dir = join(repoRoot, MIGRATIONS_DIR);
  const seen = new Map<number, string>();
  const out: Migration[] = [];
  try {
    for await (const entry of Deno.readDir(dir)) {
      if (!entry.isFile) continue;
      if (entry.name === ".gitkeep") continue;
      const m = FILENAME_RE.exec(entry.name);
      if (!m) continue;
      const seq = parseInt(m[1], 10);
      if (seen.has(seq)) {
        throw new Error(
          `Duplicate migration sequence ${seq.toString().padStart(4, "0")}: ` +
            `${seen.get(seq)} and ${entry.name}`,
        );
      }
      seen.set(seq, entry.name);
      const text = await Deno.readTextFile(join(dir, entry.name));
      const parsed = parseYaml(text) as Partial<Migration> & {
        operations?: Operation[];
      };
      if (!parsed.version_from || !parsed.version_to || !parsed.description) {
        throw new Error(
          `Migration ${entry.name} missing required fields (version_from, version_to, description)`,
        );
      }
      out.push({
        path: `${MIGRATIONS_DIR}/${entry.name}`,
        seq,
        version_from: parsed.version_from,
        version_to: parsed.version_to,
        description: parsed.description,
        operations: parsed.operations ?? [],
      });
    }
  } catch (err) {
    if (err instanceof Deno.errors.NotFound) return [];
    throw err;
  }
  out.sort((a, b) => a.seq - b.seq);

  // Detect gaps (1, 2, 3 — not 1, 3).
  for (let i = 0; i < out.length; i++) {
    const expected = i + 1;
    if (out[i].seq !== expected) {
      throw new Error(
        `Migration sequence gap: expected ${expected.toString().padStart(4, "0")} but found ${
          out[i].seq.toString().padStart(4, "0")
        } (${out[i].path})`,
      );
    }
  }
  return out;
}

// ─── Version helpers ─────────────────────────────────────────────────────────

interface Semver {
  major: number;
  minor: number;
  patch: number;
}

function parseSemver(v: string): Semver {
  const m = /^(\d+)\.(\d+)\.(\d+)$/.exec(v.trim());
  if (!m) throw new Error(`Invalid semver: '${v}'`);
  return { major: +m[1], minor: +m[2], patch: +m[3] };
}

function cmpSemver(a: Semver, b: Semver): number {
  if (a.major !== b.major) return a.major - b.major;
  if (a.minor !== b.minor) return a.minor - b.minor;
  return a.patch - b.patch;
}

// ─── Schema I/O ──────────────────────────────────────────────────────────────

// endpoint_rules: map of relation → array of {source_kind, target_kind} pairs
type EndpointRule = { source_kind: string; target_kind: string };

interface SchemaFile {
  format_version: string;
  ontology_version?: string;
  khive_version?: string;
  entity_kinds?: string[];
  edge_relations?: Array<{ relation: string; category?: string; description?: string }>;
  // Declared per-kind property schemas: kind → {name, type, required, description?}[]
  entity_properties?: Record<
    string,
    Array<{ name: string; type: string; required?: boolean; description?: string }>
  >;
  // Declared endpoint extension rules: relation → [{source_kind, target_kind}]
  endpoint_rules?: Record<string, EndpointRule[]>;
  remotes?: unknown;
  [key: string]: unknown;
}

async function loadSchemaFile(repoRoot: string): Promise<SchemaFile> {
  const path = join(repoRoot, SCHEMA_FILE);
  const text = await Deno.readTextFile(path);
  const parsed = parseYaml(text) as SchemaFile;
  if (!parsed.format_version) {
    throw new Error(`${SCHEMA_FILE} missing format_version`);
  }
  if (!parsed.ontology_version) {
    parsed.ontology_version = "1.0.0";
  }
  return parsed;
}

// ─── NDJSON rewriter ─────────────────────────────────────────────────────────

interface RewriteCounts {
  entitiesScanned: number;
  entitiesRewritten: number;
  entitiesAborted: string[];
  edgesScanned: number;
  edgesRewritten: number;
  edgesAborted: string[];
  edgesDropped: number;
}

function emptyCounts(): RewriteCounts {
  return {
    entitiesScanned: 0,
    entitiesRewritten: 0,
    entitiesAborted: [],
    edgesScanned: 0,
    edgesRewritten: 0,
    edgesAborted: [],
    edgesDropped: 0,
  };
}

async function readNdjsonLines(repoRoot: string, rel: string): Promise<string[]> {
  try {
    const text = await Deno.readTextFile(join(repoRoot, rel));
    return text.split("\n")
      .map((line) => line.trim())
      .filter((line) => line.length > 0);
  } catch (err) {
    if (err instanceof Deno.errors.NotFound) return [];
    throw err;
  }
}

// ─── Operation applier ──────────────────────────────────────────────────────

interface ApplyState {
  schema: SchemaFile;
  entities: string[];
  edges: string[];
  counts: RewriteCounts;
}

/**
 * Build a map of entity ID → kind from the current entity lines.
 * Used for endpoint-kind filtering in remove_relation_endpoint.
 */
function buildEntityKindMap(entities: string[]): Map<string, string> {
  const map = new Map<string, string>();
  for (const raw of entities) {
    try {
      const obj = JSON.parse(raw);
      if (obj.id && obj.kind) {
        map.set(String(obj.id), String(obj.kind));
      }
    } catch {
      // skip unparseable lines
    }
  }
  return map;
}

function applyOperation(state: ApplyState, op: Operation): void {
  if ("add_kind" in op) {
    const { name } = op.add_kind;
    // Validate name format: must be a non-empty lowercase identifier.
    // Per ADR-001 the base taxonomy (BASE_ENTITY_KINDS) is closed; add_kind is
    // for pack-backed extension kinds. Base kinds are always valid and do not
    // need to appear in entity_kinds, but adding them again is harmless.
    if (!name || typeof name !== "string" || !/^[a-z][a-z0-9_-]*$/.test(name)) {
      throw new Error(
        `add_kind: invalid kind name '${name}'. ` +
          `Kind names must be lowercase and match /^[a-z][a-z0-9_-]*$/. ` +
          `Base kinds (${[...BASE_ENTITY_KINDS].join(", ")}) are always valid without add_kind.`,
      );
    }
    state.schema.entity_kinds = state.schema.entity_kinds ?? [];
    if (!state.schema.entity_kinds.includes(name)) {
      state.schema.entity_kinds.push(name);
    }
    return;
  }

  if ("remove_kind" in op) {
    const { name, on_existing, target } = op.remove_kind;
    const matched: string[] = [];
    state.entities = state.entities.map((raw) => {
      state.counts.entitiesScanned++;
      try {
        const obj = JSON.parse(raw);
        if (obj.kind === name) {
          matched.push(obj.id as string);
          if (on_existing === "migrate_to") {
            if (!target) {
              throw new Error(`remove_kind '${name}' migrate_to requires 'target'`);
            }
            obj.kind = target;
            state.counts.entitiesRewritten++;
            return JSON.stringify(obj);
          }
        }
      } catch {
        // Not parseable as JSON; keep as-is.
      }
      return raw;
    });
    if (on_existing === "error" && matched.length > 0) {
      state.counts.entitiesAborted.push(
        `remove_kind ${name}: ${matched.length} entities still exist (on_existing=error)`,
      );
      throw new Error(
        `remove_kind '${name}' aborted: ${matched.length} entities still exist`,
      );
    }
    if (state.schema.entity_kinds) {
      state.schema.entity_kinds = state.schema.entity_kinds.filter((k) => k !== name);
    }
    // Remove any property schema entries for this kind.
    if (state.schema.entity_properties) {
      delete state.schema.entity_properties[name];
    }
    return;
  }

  if ("rename_kind" in op) {
    const { from, to } = op.rename_kind;
    state.entities = state.entities.map((raw) => {
      state.counts.entitiesScanned++;
      try {
        const obj = JSON.parse(raw);
        if (obj.kind === from) {
          obj.kind = to;
          state.counts.entitiesRewritten++;
          return JSON.stringify(obj);
        }
      } catch {
        // ignore
      }
      return raw;
    });
    if (state.schema.entity_kinds) {
      state.schema.entity_kinds = state.schema.entity_kinds.map((k) => k === from ? to : k);
    }
    // Rename in property schema map.
    if (state.schema.entity_properties && state.schema.entity_properties[from] !== undefined) {
      state.schema.entity_properties[to] = state.schema.entity_properties[from];
      delete state.schema.entity_properties[from];
    }
    return;
  }

  if ("add_property" in op) {
    const { kind, name, type, required, description } = op.add_property;
    if (required) {
      // Abort if any entity of this kind lacks the property.
      const missing: string[] = [];
      for (const raw of state.entities) {
        state.counts.entitiesScanned++;
        try {
          const obj = JSON.parse(raw);
          if (obj.kind === kind) {
            const props = (obj.properties ?? {}) as Record<string, unknown>;
            if (!(name in props)) missing.push(obj.id as string);
          }
        } catch {
          // ignore
        }
      }
      if (missing.length > 0) {
        const sample = missing.slice(0, 3).join(", ");
        throw new Error(
          `add_property '${name}' on '${kind}' required=true aborted: ` +
            `${missing.length} entities lack the property (e.g. ${sample})`,
        );
      }
    }
    // Record the property in the schema's entity_properties map.
    state.schema.entity_properties = state.schema.entity_properties ?? {};
    state.schema.entity_properties[kind] = state.schema.entity_properties[kind] ?? [];
    const existing = state.schema.entity_properties[kind].find((p) => p.name === name);
    if (!existing) {
      state.schema.entity_properties[kind].push({
        name,
        type,
        ...(required !== undefined ? { required } : {}),
        ...(description ? { description } : {}),
      });
    }
    return;
  }

  if ("remove_property" in op) {
    const { kind, name } = op.remove_property;
    // Remove the property from the schema's entity_properties map.
    if (state.schema.entity_properties?.[kind]) {
      state.schema.entity_properties[kind] = state.schema.entity_properties[kind].filter(
        (p) => p.name !== name,
      );
      if (state.schema.entity_properties[kind].length === 0) {
        delete state.schema.entity_properties[kind];
      }
    }
    // No NDJSON rewrite — previously set values are retained but no longer schema-validated.
    return;
  }

  if ("rename_property" in op) {
    const { kind, from, to } = op.rename_property;
    state.entities = state.entities.map((raw) => {
      state.counts.entitiesScanned++;
      try {
        const obj = JSON.parse(raw);
        if (
          obj.kind === kind && obj.properties && typeof obj.properties === "object" &&
          !Array.isArray(obj.properties)
        ) {
          const props = obj.properties as Record<string, unknown>;
          if (from in props) {
            props[to] = props[from];
            delete props[from];
            state.counts.entitiesRewritten++;
            return JSON.stringify(obj);
          }
        }
      } catch {
        // ignore
      }
      return raw;
    });
    // Update the property name in the schema's entity_properties map.
    if (state.schema.entity_properties?.[kind]) {
      state.schema.entity_properties[kind] = state.schema.entity_properties[kind].map((p) =>
        p.name === from ? { ...p, name: to } : p
      );
    }
    return;
  }

  if ("add_relation_endpoint" in op) {
    const { relation, source_kind, target_kind } = op.add_relation_endpoint;
    // Validate that the relation is one of the 13 canonical ADR-002 relations.
    if (!CANONICAL_RELATIONS.has(relation)) {
      throw new Error(
        `add_relation_endpoint: unknown relation '${relation}'. ` +
          `Must be one of the 13 canonical ADR-002 relations: ${
            [...CANONICAL_RELATIONS].join(", ")
          }.`,
      );
    }
    // Record the endpoint rule in the schema.
    state.schema.endpoint_rules = state.schema.endpoint_rules ?? {};
    state.schema.endpoint_rules[relation] = state.schema.endpoint_rules[relation] ?? [];
    const already = state.schema.endpoint_rules[relation].some(
      (r) => r.source_kind === source_kind && r.target_kind === target_kind,
    );
    if (!already) {
      state.schema.endpoint_rules[relation].push({ source_kind, target_kind });
    }
    // No NDJSON rewrite — new endpoints become accepted immediately.
    return;
  }

  if ("remove_relation_endpoint" in op) {
    const { relation, source_kind, target_kind, on_existing } = op.remove_relation_endpoint;
    // Validate that the relation is one of the 13 canonical ADR-002 relations.
    if (!CANONICAL_RELATIONS.has(relation)) {
      throw new Error(
        `remove_relation_endpoint: unknown relation '${relation}'. ` +
          `Must be one of the 13 canonical ADR-002 relations: ${
            [...CANONICAL_RELATIONS].join(", ")
          }.`,
      );
    }
    // Build entity ID → kind map to match endpoint pairs exactly.
    const entityKindMap = buildEntityKindMap(state.entities);
    const matched: string[] = [];
    const filtered: string[] = [];
    for (const raw of state.edges) {
      state.counts.edgesScanned++;
      try {
        const obj = JSON.parse(raw);
        if (obj.relation === relation) {
          // Resolve actual kinds for both endpoints.
          const srcKind = entityKindMap.get(String(obj.source)) ?? "unknown";
          const tgtKind = entityKindMap.get(String(obj.target)) ?? "unknown";
          // Match: source_kind/target_kind of "any" is a wildcard (for backward compat
          // with schemas that don't track entity kinds per edge).
          const srcMatch = source_kind === "any" || srcKind === source_kind;
          const tgtMatch = target_kind === "any" || tgtKind === target_kind;
          if (srcMatch && tgtMatch) {
            matched.push(`${obj.source} -> ${obj.target} (${obj.relation})`);
            if (on_existing === "drop") {
              state.counts.edgesDropped++;
              continue;
            }
          }
        }
      } catch {
        // ignore
      }
      filtered.push(raw);
    }
    if (on_existing === "error" && matched.length > 0) {
      state.counts.edgesAborted.push(
        `remove_relation_endpoint ${relation}(${source_kind},${target_kind}): ${matched.length} edges still exist`,
      );
      throw new Error(
        `remove_relation_endpoint '${relation}' (${source_kind}→${target_kind}) aborted: ${matched.length} edges still exist`,
      );
    }
    state.edges = filtered;
    // Remove the endpoint rule from the schema.
    if (state.schema.endpoint_rules?.[relation]) {
      state.schema.endpoint_rules[relation] = state.schema.endpoint_rules[relation].filter(
        (r) => !(r.source_kind === source_kind && r.target_kind === target_kind),
      );
      if (state.schema.endpoint_rules[relation].length === 0) {
        delete state.schema.endpoint_rules[relation];
      }
      // If the entire endpoint_rules map is now empty, remove the key entirely
      // so schema.yaml doesn't carry an empty object.
      if (
        state.schema.endpoint_rules !== undefined &&
        Object.keys(state.schema.endpoint_rules).length === 0
      ) {
        delete state.schema.endpoint_rules;
      }
    }
    return;
  }

  throw new Error(`Unknown operation: ${JSON.stringify(op)}`);
}

// ─── Migration applier ──────────────────────────────────────────────────────

export interface ApplyResult {
  applied: Migration[];
  skipped: Migration[];
  counts: RewriteCounts;
  finalVersion: string;
}

export async function applyMigrations(
  repoRoot: string,
  options: { toVersion?: string; dryRun?: boolean } = {},
): Promise<ApplyResult> {
  const schema = await loadSchemaFile(repoRoot);
  const currentVersion = parseSemver(schema.ontology_version ?? "1.0.0");
  const target = options.toVersion ? parseSemver(options.toVersion) : null;

  const migrations = await listMigrationFiles(repoRoot);
  const applied: Migration[] = [];
  const skipped: Migration[] = [];

  const state: ApplyState = {
    schema,
    entities: await readNdjsonLines(repoRoot, ENTITIES_FILE),
    edges: await readNdjsonLines(repoRoot, EDGES_FILE),
    counts: emptyCounts(),
  };

  let activeVersion = currentVersion;

  for (const mig of migrations) {
    const from = parseSemver(mig.version_from);
    const to = parseSemver(mig.version_to);

    if (cmpSemver(from, activeVersion) < 0) {
      // version_from < current => already applied
      skipped.push(mig);
      continue;
    }
    if (cmpSemver(from, activeVersion) > 0) {
      throw new Error(
        `Migration ${mig.path} expects version_from=${mig.version_from} but current is ${
          formatSemver(activeVersion)
        }`,
      );
    }
    if (target && cmpSemver(to, target) > 0) {
      // Past the requested target.
      break;
    }

    for (const op of mig.operations) {
      applyOperation(state, op);
    }
    state.schema.ontology_version = mig.version_to;
    activeVersion = to;
    applied.push(mig);
  }

  if (!options.dryRun && applied.length > 0) {
    // Atomicity: stage all writes to temp paths, then rename into place.
    // If any temp write fails, we never touch the real files and throw.
    const schemaPath = join(repoRoot, SCHEMA_FILE);
    const entitiesPath = join(repoRoot, ENTITIES_FILE);
    const edgesPath = join(repoRoot, EDGES_FILE);

    const schemaTmp = schemaPath + ".migrate_tmp";
    const entitiesTmp = entitiesPath + ".migrate_tmp";
    const edgesTmp = edgesPath + ".migrate_tmp";

    const needEntities = state.counts.entitiesRewritten > 0;
    const needEdges = state.counts.edgesDropped > 0;

    // Stage phase: write all temps first.
    const schemaText = stringifyYaml(state.schema as unknown as Record<string, unknown>);
    await Deno.writeTextFile(schemaTmp, schemaText);
    if (needEntities) {
      const text = state.entities.join("\n") + (state.entities.length > 0 ? "\n" : "");
      await Deno.writeTextFile(entitiesTmp, text);
    }
    if (needEdges) {
      const text = state.edges.join("\n") + (state.edges.length > 0 ? "\n" : "");
      await Deno.writeTextFile(edgesTmp, text);
    }

    // Commit phase: rename all temps into place atomically (best-effort on non-POSIX;
    // Deno.rename is atomic on POSIX filesystems).
    try {
      await Deno.rename(schemaTmp, schemaPath);
      if (needEntities) await Deno.rename(entitiesTmp, entitiesPath);
      if (needEdges) await Deno.rename(edgesTmp, edgesPath);
    } catch (err) {
      // Best-effort rollback: clean up any remaining temp files.
      for (const tmp of [schemaTmp, entitiesTmp, edgesTmp]) {
        try {
          await Deno.remove(tmp);
        } catch { /* ignore */ }
      }
      throw err;
    }
  }

  return {
    applied,
    skipped,
    counts: state.counts,
    finalVersion: formatSemver(activeVersion),
  };
}

function formatSemver(s: Semver): string {
  return `${s.major}.${s.minor}.${s.patch}`;
}

// ─── CLI entry ──────────────────────────────────────────────────────────────

function printHelp(): void {
  console.log(`Usage: khive kg migrate [options]

Apply pending schema migrations from .khive/kg/migrations/ in sequence.

Options:
  --dry-run             Print what would change without writing.
  --to <version>        Stop after the migration whose version_to matches.
  --list                List pending and applied migrations; no apply.
  -h, --help            Show this help.

Reference: ADR-054 — KG Schema Evolution.`);
}

interface CliArgs {
  dryRun: boolean;
  to?: string;
  list: boolean;
}

function parseArgs(args: string[]): CliArgs {
  const out: CliArgs = { dryRun: false, list: false };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "--dry-run") out.dryRun = true;
    else if (a === "--list") out.list = true;
    else if (a === "--to" && args[i + 1]) {
      out.to = args[i + 1];
      i++;
    } else if (a.startsWith("--to=")) {
      out.to = a.slice("--to=".length);
    }
  }
  return out;
}

export async function runMigrate(
  repoRoot: string,
  args: string[],
): Promise<number> {
  if (args.includes("--help") || args.includes("-h")) {
    printHelp();
    return 0;
  }

  const opts = parseArgs(args);

  if (opts.list) {
    const schema = await loadSchemaFile(repoRoot);
    const current = parseSemver(schema.ontology_version ?? "1.0.0");
    const migrations = await listMigrationFiles(repoRoot);
    console.log(`Current ontology_version: ${formatSemver(current)}`);
    if (migrations.length === 0) {
      console.log("No migrations defined.");
      return 0;
    }
    for (const m of migrations) {
      const from = parseSemver(m.version_from);
      const applied = cmpSemver(from, current) < 0;
      const status = applied ? "applied " : "pending ";
      console.log(
        `  ${status} ${
          m.path.replace(MIGRATIONS_DIR + "/", "")
        }  ${m.version_from} → ${m.version_to}  ${m.description}`,
      );
    }
    return 0;
  }

  let result: ApplyResult;
  try {
    result = await applyMigrations(repoRoot, {
      toVersion: opts.to,
      dryRun: opts.dryRun,
    });
  } catch (err) {
    console.error(`Migration failed: ${(err as Error).message}`);
    return 1;
  }

  if (result.applied.length === 0) {
    console.log(`No pending migrations. ontology_version: ${result.finalVersion}.`);
    if (result.skipped.length > 0) {
      console.log(`  (${result.skipped.length} already-applied migrations skipped)`);
    }
    return 0;
  }

  const verb = opts.dryRun ? "Would apply" : "Applied";
  console.log(`${verb} ${result.applied.length} migration(s):`);
  for (const m of result.applied) {
    console.log(
      `  ${
        m.path.replace(MIGRATIONS_DIR + "/", "")
      }  ${m.version_from} → ${m.version_to}  ${m.description}`,
    );
  }
  console.log(
    `  entities: scanned ${result.counts.entitiesScanned}, rewritten ${result.counts.entitiesRewritten}`,
  );
  if (result.counts.edgesScanned > 0 || result.counts.edgesDropped > 0) {
    console.log(
      `  edges: scanned ${result.counts.edgesScanned}, dropped ${result.counts.edgesDropped}`,
    );
  }
  console.log(`  ontology_version: ${result.finalVersion}`);

  if (opts.dryRun) {
    console.log("");
    console.log("(--dry-run — no files written)");
  } else {
    console.log("");
    console.log(
      "Migration complete. Run 'git add .khive/kg/' and 'git commit' to record.",
    );
  }
  return 0;
}
