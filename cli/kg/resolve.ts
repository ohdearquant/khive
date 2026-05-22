/**
 * `khive kg resolve` — entity-level merge conflict resolution (ADR-053).
 *
 * After `git merge` produces conflict markers in `entities.ndjson` or
 * `edges.ndjson`, this command:
 *
 *   1. Scans `schema.yaml`, `entities.ndjson`, and `edges.ndjson` for
 *      conflict markers. If `schema.yaml` has conflicts, prints an error and
 *      exits 1 (ADR-053 §5 — schema conflicts require manual review).
 *   2. Applies a resolution strategy:
 *        --ours              keep current branch lines
 *        --theirs            keep incoming branch lines
 *        --merge-properties  union of non-overlapping properties; FAIL on
 *                            overlapping keys unless an explicit per-record
 *                            override is supplied (ADR-053 §3)
 *   3. Refuses to sort or write a file that still contains manual conflict
 *      markers — prevents corrupting unresolved blocks (ADR-053 §3 step 3).
 *   4. Stages resolved content to temp files on the SAME filesystem, then
 *      runs `khive kg validate` on the staged graph. Only on validation pass
 *      are the originals atomically replaced (ADR-053 §3 steps 4-5).
 *   5. Prints a summary and exits 0 if resolved, 1 if conflicts remain
 *      after resolution or validation fails.
 *
 * Per-record overrides (ADR-053 §3 and §4):
 *   --entity <id> --ours|--theirs|--manual
 *   --edge <source> <target> <relation> --ours|--theirs
 *
 * `--manual` leaves the conflict markers in place for that record so the
 * user can edit it manually before re-running resolve.
 */

import { EDGE_RELATIONS, ENTITY_KINDS, parseEdgeLine, parseEntityLine } from "../lib/ndjson.ts";
import { EDGES_FILE, ENTITIES_FILE, SCHEMA_FILE } from "../lib/paths.ts";
import { printValidationResult, validate } from "./validate.ts";
import { join } from "@std/path";

// ─── Types ────────────────────────────────────────────────────────────────────

type Strategy = "ours" | "theirs" | "merge-properties";

interface ResolveArgs {
  strategy: Strategy;
  entityOverrides: Map<string, "ours" | "theirs" | "manual">;
  edgeOverrides: Map<string, "ours" | "theirs">;
  dryRun: boolean;
}

interface ConflictBlock {
  /** 0-based start line of `<<<<<<<` */
  start: number;
  /** 0-based end line of `>>>>>>>` */
  end: number;
  /** "ours" side lines (without markers). */
  ours: string[];
  /** "theirs" side lines (without markers). */
  theirs: string[];
  /** Label after `>>>>>>>` (branch / commit name). */
  theirsLabel: string;
}

interface ResolutionStats {
  entityConflicts: number;
  edgeConflicts: number;
  resolvedEntities: number;
  resolvedEdges: number;
  manualLeft: number;
  warnings: string[];
}

// ─── Argument parsing ─────────────────────────────────────────────────────────

function parseArgs(args: string[]): ResolveArgs {
  const out: ResolveArgs = {
    strategy: "ours",
    entityOverrides: new Map(),
    edgeOverrides: new Map(),
    dryRun: false,
  };

  // Detect global strategy.
  for (const a of args) {
    if (a === "--ours") out.strategy = "ours";
    else if (a === "--theirs") out.strategy = "theirs";
    else if (a === "--merge-properties") out.strategy = "merge-properties";
    else if (a === "--dry-run") out.dryRun = true;
  }

  // Detect per-record overrides.
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "--entity") {
      const id = args[i + 1];
      const verb = args[i + 2];
      if (!id || !verb) continue;
      if (verb === "--ours") out.entityOverrides.set(id, "ours");
      else if (verb === "--theirs") out.entityOverrides.set(id, "theirs");
      else if (verb === "--manual") out.entityOverrides.set(id, "manual");
      i += 2;
    } else if (a === "--edge") {
      // ADR-053 §4: --edge <source> <target> <relation> --ours|--theirs
      const source = args[i + 1];
      const target = args[i + 2];
      const relation = args[i + 3];
      const verb = args[i + 4];
      if (!source || !target || !relation || !verb) {
        // Not enough tokens — skip without consuming extras so we don't
        // misparse a subsequent flag as a relation.
        continue;
      }
      const key = `${source}:${target}:${relation}`;
      if (verb === "--ours") out.edgeOverrides.set(key, "ours");
      else if (verb === "--theirs") out.edgeOverrides.set(key, "theirs");
      i += 4;
    }
  }

  return out;
}

// ─── Conflict marker parser ───────────────────────────────────────────────────

const CONFLICT_START = /^<{7}(?: .*)?$/;
const CONFLICT_MID = /^={7}(?: .*)?$/;
const CONFLICT_END = /^>{7}(?: .*)?$/;

/**
 * Scan `lines` for conflict blocks. Returns the list of blocks plus the
 * indices of lines that lie OUTSIDE any conflict (so the file can be
 * rebuilt with chosen resolutions).
 */
export function parseConflicts(
  lines: string[],
): { blocks: ConflictBlock[]; cleanLines: string[] } {
  const blocks: ConflictBlock[] = [];
  const cleanLines: string[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (CONFLICT_START.test(line)) {
      const start = i;
      const ours: string[] = [];
      i++;
      while (i < lines.length && !CONFLICT_MID.test(lines[i])) {
        if (CONFLICT_START.test(lines[i]) || CONFLICT_END.test(lines[i])) {
          throw new Error(
            `Malformed conflict block at line ${start + 1}: nested marker found`,
          );
        }
        ours.push(lines[i]);
        i++;
      }
      if (i >= lines.length) {
        throw new Error(
          `Unterminated conflict block starting at line ${start + 1} (no === separator)`,
        );
      }
      i++; // skip ===
      const theirs: string[] = [];
      while (i < lines.length && !CONFLICT_END.test(lines[i])) {
        theirs.push(lines[i]);
        i++;
      }
      if (i >= lines.length) {
        throw new Error(
          `Unterminated conflict block starting at line ${start + 1} (no >>>>>>> end)`,
        );
      }
      const theirsLabel = lines[i].slice(8).trim();
      blocks.push({ start, end: i, ours, theirs, theirsLabel });
      i++; // skip >>>
    } else {
      cleanLines.push(line);
      i++;
    }
  }
  return { blocks, cleanLines };
}

// Multiline versions for whole-file marker scanning (hasConflictMarkers).
// The line-level constants above use `^` as start-of-string (single line);
// these use the `m` flag so `^` matches start of any line in the full text.
const CONFLICT_START_M = /^<{7}(?: .*)?$/m;
const CONFLICT_MID_M = /^={7}(?: .*)?$/m;
const CONFLICT_END_M = /^>{7}(?: .*)?$/m;

/**
 * Returns true if `text` contains any git conflict markers.
 * Used to detect schema.yaml and residual NDJSON conflicts without a full
 * parse — a marker anywhere in the content means the file is unresolved.
 */
export function hasConflictMarkers(text: string): boolean {
  return CONFLICT_START_M.test(text) || CONFLICT_MID_M.test(text) ||
    CONFLICT_END_M.test(text);
}

// ─── Entity / edge identity ───────────────────────────────────────────────────

interface EntityKey {
  id: string;
}

interface EdgeKey {
  source: string;
  target: string;
  relation: string;
}

/**
 * Inspect the lines in a conflict side and try to extract an entity id
 * (NDJSON record). Returns the first valid entity id, or null.
 */
function extractEntityKey(lines: string[]): EntityKey | null {
  for (const raw of lines) {
    const trimmed = raw.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    try {
      const obj = JSON.parse(trimmed);
      const e = parseEntityLine(obj);
      if (e) return { id: e.id };
    } catch {
      // fall through
    }
  }
  return null;
}

function extractEdgeKey(lines: string[]): EdgeKey | null {
  for (const raw of lines) {
    const trimmed = raw.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    try {
      const obj = JSON.parse(trimmed);
      const e = parseEdgeLine(obj);
      if (e) return { source: e.source, target: e.target, relation: e.relation };
    } catch {
      // fall through
    }
  }
  return null;
}

function edgeKeyToString(k: EdgeKey): string {
  return `${k.source}:${k.target}:${k.relation}`;
}

/**
 * Detect a delete-vs-edit conflict: one side has a parseable record, the
 * other is empty (or contains only whitespace / comment lines). This is a
 * data-loss decision that ADR-053 §4 says must require explicit override.
 */
function isDeleteVsEdit(oursLines: string[], theirsLines: string[]): boolean {
  const oursHasRecord = oursLines.some((l) => {
    const t = l.trim();
    return t && !t.startsWith("#");
  });
  const theirsHasRecord = theirsLines.some((l) => {
    const t = l.trim();
    return t && !t.startsWith("#");
  });
  // One side has content and the other does not → delete-vs-edit.
  return oursHasRecord !== theirsHasRecord;
}

// ─── Property merge ───────────────────────────────────────────────────────────

interface MergeResult {
  merged: Record<string, unknown>;
  overlapping: string[];
}

function mergeProperties(
  ours: Record<string, unknown>,
  theirs: Record<string, unknown>,
): MergeResult {
  const merged = { ...ours };
  const overlapping: string[] = [];
  for (const [k, v] of Object.entries(theirs)) {
    if (k in merged) {
      const ourValue = JSON.stringify(merged[k]);
      const theirValue = JSON.stringify(v);
      if (ourValue !== theirValue) {
        overlapping.push(k);
      }
      // current branch (ours) wins on overlap (per ADR-053 §3)
    } else {
      merged[k] = v;
    }
  }
  return { merged, overlapping };
}

// ─── Resolution strategy application ──────────────────────────────────────────

/**
 * Resolve a single conflict block according to the effective strategy.
 *
 * ADR-053 §3 — `--merge-properties` must FAIL loudly on overlapping keys
 * (not just warn) unless the caller has supplied an explicit per-record
 * override. `isExplicitOverride` is true when the caller already decided
 * the strategy via `--entity <id> --ours|--theirs`.
 *
 * ADR-053 §4 — delete-vs-edit conflicts require explicit `--ours` or
 * `--theirs`; `--merge-properties` cannot resolve them.
 */
function chooseSide(
  strategy: "ours" | "theirs" | "merge-properties",
  override: "ours" | "theirs" | "manual" | undefined,
  ours: string[],
  theirs: string[],
): { lines: string[]; warnings: string[]; manual: boolean; error?: string } {
  const effective = override ?? strategy;
  if (effective === "manual") {
    return {
      lines: [
        "<<<<<<< HEAD",
        ...ours,
        "=======",
        ...theirs,
        ">>>>>>> incoming",
      ],
      warnings: [],
      manual: true,
    };
  }

  if (effective === "ours") return { lines: ours, warnings: [], manual: false };
  if (effective === "theirs") return { lines: theirs, warnings: [], manual: false };

  // merge-properties path — only meaningful when NO explicit per-record
  // override was given (override === undefined at this point).

  // Delete-vs-edit: one side is empty. ADR-053 §4 says this requires
  // explicit --ours / --theirs; refuse to auto-resolve.
  if (isDeleteVsEdit(ours, theirs)) {
    return {
      lines: [
        "<<<<<<< HEAD",
        ...ours,
        "=======",
        ...theirs,
        ">>>>>>> incoming",
      ],
      warnings: [],
      manual: true,
      error: "delete-vs-edit conflict requires explicit --ours or --theirs override (ADR-053 §4)",
    };
  }

  // Multi-record sides: cannot merge properties across multiple records.
  if (ours.length !== 1 || theirs.length !== 1) {
    return {
      lines: [
        "<<<<<<< HEAD",
        ...ours,
        "=======",
        ...theirs,
        ">>>>>>> incoming",
      ],
      warnings: [],
      manual: true,
      error: "merge-properties: multi-line conflict requires explicit --ours or --theirs override",
    };
  }

  // Single-record merge.
  let oursObj: Record<string, unknown>;
  let theirsObj: Record<string, unknown>;
  try {
    oursObj = JSON.parse(ours[0]);
    theirsObj = JSON.parse(theirs[0]);
  } catch {
    // Malformed JSON — cannot merge; require explicit override.
    return {
      lines: [
        "<<<<<<< HEAD",
        ...ours,
        "=======",
        ...theirs,
        ">>>>>>> incoming",
      ],
      warnings: [],
      manual: true,
      error: "merge-properties: malformed JSON requires explicit --ours or --theirs override",
    };
  }

  const out: Record<string, unknown> = { ...oursObj };
  let overlapAll: string[] = [];

  // Properties — special-case merge.
  if (
    oursObj.properties && typeof oursObj.properties === "object" &&
    !Array.isArray(oursObj.properties) &&
    theirsObj.properties && typeof theirsObj.properties === "object" &&
    !Array.isArray(theirsObj.properties)
  ) {
    const m = mergeProperties(
      oursObj.properties as Record<string, unknown>,
      theirsObj.properties as Record<string, unknown>,
    );
    out.properties = m.merged;
    overlapAll = m.overlapping;
  }

  // ADR-053 §3: fail loudly on overlapping keys — leave as manual conflict
  // so the user must supply an explicit per-record override.
  if (overlapAll.length > 0) {
    return {
      lines: [
        "<<<<<<< HEAD",
        ...ours,
        "=======",
        ...theirs,
        ">>>>>>> incoming",
      ],
      warnings: [],
      manual: true,
      error: `merge-properties: overlapping property keys require explicit per-record override: ${
        overlapAll.join(", ")
      } (use --entity <id> --ours|--theirs)`,
    };
  }

  // Tags — union of arrays.
  if (Array.isArray(oursObj.tags) && Array.isArray(theirsObj.tags)) {
    const tags = new Set<string>([
      ...oursObj.tags.filter((t): t is string => typeof t === "string"),
      ...theirsObj.tags.filter((t): t is string => typeof t === "string"),
    ]);
    out.tags = Array.from(tags).sort();
  }

  return { lines: [JSON.stringify(out)], warnings: [], manual: false };
}

// ─── Per-file resolution ──────────────────────────────────────────────────────

interface FileResolution {
  newLines: string[];
  resolved: number;
  manualLeft: number;
  warnings: string[];
  errors: string[];
}

function resolveLines(
  fileLabel: string,
  lines: string[],
  args: ResolveArgs,
  kind: "entities" | "edges",
): FileResolution {
  const { blocks } = parseConflicts(lines);
  if (blocks.length === 0) {
    return { newLines: lines, resolved: 0, manualLeft: 0, warnings: [], errors: [] };
  }

  let resolved = 0;
  let manualLeft = 0;
  const warnings: string[] = [];
  const errors: string[] = [];

  // Rebuild file: walk lines, swap each conflict block in turn.
  const out: string[] = [];
  let i = 0;
  let blockIdx = 0;
  while (i < lines.length) {
    if (blockIdx < blocks.length && i === blocks[blockIdx].start) {
      const block = blocks[blockIdx];

      // Determine per-record override.
      let override: "ours" | "theirs" | "manual" | undefined;
      if (kind === "entities") {
        const oursKey = extractEntityKey(block.ours);
        const theirsKey = extractEntityKey(block.theirs);
        const key = oursKey?.id ?? theirsKey?.id;
        if (key) {
          override = args.entityOverrides.get(key) ??
            args.entityOverrides.get(key.slice(0, 8));
        }
      } else {
        const oursKey = extractEdgeKey(block.ours);
        const theirsKey = extractEdgeKey(block.theirs);
        const key = oursKey ?? theirsKey;
        if (key) {
          override = args.edgeOverrides.get(edgeKeyToString(key));
        }
      }

      const decision = chooseSide(args.strategy, override, block.ours, block.theirs);
      for (const ln of decision.lines) out.push(ln);
      for (const w of decision.warnings) {
        warnings.push(`${fileLabel}: ${w}`);
      }
      if (decision.error) {
        errors.push(`${fileLabel}: ${decision.error}`);
      }
      if (decision.manual) manualLeft++;
      else resolved++;

      i = block.end + 1;
      blockIdx++;
    } else {
      out.push(lines[i]);
      i++;
    }
  }
  return { newLines: out, resolved, manualLeft, warnings, errors };
}

// ─── Resort ───────────────────────────────────────────────────────────────────

/**
 * Sort entities.ndjson lines by UUID (ascending).
 *
 * IMPORTANT: this function MUST only be called when the file has NO
 * remaining conflict markers. If manual conflict blocks are present the
 * JSON lines inside them would be sorted out of their block context,
 * corrupting the conflict. The caller is responsible for checking
 * `manualLeft === 0` before calling.
 */
function sortEntitiesNdjson(lines: string[]): string[] {
  const records: { id: string; line: string }[] = [];
  const passthrough: string[] = [];
  for (const raw of lines) {
    const trimmed = raw.trim();
    if (!trimmed) continue;
    if (trimmed.startsWith("#")) {
      passthrough.push(raw);
      continue;
    }
    try {
      const obj = JSON.parse(trimmed);
      const e = parseEntityLine(obj);
      if (e) records.push({ id: e.id.toLowerCase(), line: trimmed });
      else passthrough.push(raw);
    } catch {
      passthrough.push(raw);
    }
  }
  records.sort((a, b) => a.id.localeCompare(b.id));
  return [...passthrough, ...records.map((r) => r.line)];
}

/**
 * Sort edges.ndjson lines by composite key (source, target, relation) ascending.
 *
 * Same caveat as sortEntitiesNdjson — caller must guarantee no conflict
 * markers remain before calling.
 */
function sortEdgesNdjson(lines: string[]): string[] {
  const records: { key: string; line: string }[] = [];
  const passthrough: string[] = [];
  for (const raw of lines) {
    const trimmed = raw.trim();
    if (!trimmed) continue;
    if (trimmed.startsWith("#")) {
      passthrough.push(raw);
      continue;
    }
    try {
      const obj = JSON.parse(trimmed);
      const e = parseEdgeLine(obj);
      if (e) {
        const key = `${e.source}\x00${e.target}\x00${e.relation}`;
        records.push({ key, line: trimmed });
      } else passthrough.push(raw);
    } catch {
      passthrough.push(raw);
    }
  }
  records.sort((a, b) => a.key.localeCompare(b.key));
  return [...passthrough, ...records.map((r) => r.line)];
}

// ─── Atomic staging helpers ───────────────────────────────────────────────────

/**
 * Write content to a temp file on the SAME filesystem as `targetPath`.
 * Returns the temp file path.
 *
 * Placing the temp file in the same directory as the target ensures that
 * the final rename is atomic on POSIX (single-FS rename(2)). If the target
 * directory is on a different mount point from the system temp dir, a
 * cross-device rename would silently copy+delete — not atomic.
 */
async function writeStagedFile(targetPath: string, content: string): Promise<string> {
  const dir = targetPath.substring(0, targetPath.lastIndexOf("/"));
  const base = targetPath.substring(targetPath.lastIndexOf("/") + 1);
  const tmpPath = `${dir}/.${base}.tmp-${crypto.randomUUID()}`;
  await Deno.writeTextFile(tmpPath, content);
  return tmpPath;
}

/**
 * Remove a staged temp file, ignoring "not found" errors (idempotent cleanup).
 */
async function removeStagedFile(path: string): Promise<void> {
  try {
    await Deno.remove(path);
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) throw err;
  }
}

// ─── CLI entry ────────────────────────────────────────────────────────────────

function printHelp(): void {
  console.log(`Usage: khive kg resolve [strategy] [overrides...]

Strategy (mutually exclusive, default --ours):
  --ours                Keep current branch (HEAD) lines.
  --theirs              Keep incoming branch lines.
  --merge-properties    Single-line conflicts: union non-overlapping
                        properties + tags. Fails if any property key
                        appears on both sides with different values —
                        supply an explicit per-record override instead.
                        Cannot auto-resolve delete-vs-edit conflicts.

Per-record overrides:
  --entity <id> --ours|--theirs|--manual
                        Apply a different strategy to one entity id.
                        --manual leaves the conflict markers in place.
  --edge <source> <target> <relation> --ours|--theirs
                        Apply a different strategy to a specific edge
                        identified by its composite key (ADR-053 §4).

Other:
  --dry-run             Print what would be resolved without writing.

Examples:
  khive kg resolve --ours
  khive kg resolve --merge-properties
  khive kg resolve --theirs --entity abc12345 --ours
  khive kg resolve --ours --edge src-uuid dst-uuid depends_on --theirs

Reference: ADR-053 — KG Branching and Merge.

Closed sets reminder (for validation after resolve):
  entity kinds : ${ENTITY_KINDS.join(", ")}
  edge relations: ${EDGE_RELATIONS.join(", ")}`);
}

/**
 * Resolve runner. Returns 0 on full resolution, 1 if conflicts remain
 * (manual leftovers), schema conflicts were detected, or validation fails.
 * Does not call `Deno.exit` — the caller (CLI dispatch) decides.
 */
export async function runResolve(
  repoRoot: string,
  args: string[],
): Promise<number> {
  if (args.includes("--help") || args.includes("-h")) {
    printHelp();
    return 0;
  }

  const opts = parseArgs(args);
  const stats: ResolutionStats = {
    entityConflicts: 0,
    edgeConflicts: 0,
    resolvedEntities: 0,
    resolvedEdges: 0,
    manualLeft: 0,
    warnings: [],
  };

  // ── Schema conflict gate (ADR-053 §5) ─────────────────────────────────────
  // Scan schema.yaml BEFORE processing NDJSON. Any conflict marker in
  // schema.yaml means the merge is structurally unresolved — block and exit.
  const schemaPath = join(repoRoot, SCHEMA_FILE);
  try {
    const schemaText = await Deno.readTextFile(schemaPath);
    if (hasConflictMarkers(schemaText)) {
      console.error("error: schema.yaml has merge conflicts that require manual resolution");
      console.error(
        "  Edit .khive/kg/schema.yaml to resolve, then run 'khive kg validate' before committing.",
      );
      console.error(
        "  Reference: ADR-053 §5 — schema conflicts require architectural review (ADR-002).",
      );
      return 1;
    }
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) throw err;
    // schema.yaml absent — not required for resolve to proceed.
  }

  // ── Entities ──────────────────────────────────────────────────────────────
  const entitiesPath = join(repoRoot, ENTITIES_FILE);
  let entitiesText: string;
  try {
    entitiesText = await Deno.readTextFile(entitiesPath);
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) throw err;
    entitiesText = "";
  }
  const entitiesLines = entitiesText.split("\n");
  const eRes = resolveLines(
    ENTITIES_FILE,
    entitiesLines.slice(-1)[0] === "" ? entitiesLines.slice(0, -1) : entitiesLines,
    opts,
    "entities",
  );
  stats.entityConflicts = eRes.resolved + eRes.manualLeft;
  stats.resolvedEntities = eRes.resolved;
  stats.manualLeft += eRes.manualLeft;
  stats.warnings.push(...eRes.warnings);

  // ── Edges ─────────────────────────────────────────────────────────────────
  const edgesPath = join(repoRoot, EDGES_FILE);
  let edgesText: string;
  try {
    edgesText = await Deno.readTextFile(edgesPath);
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) throw err;
    edgesText = "";
  }
  const edgesLines = edgesText.split("\n");
  const xRes = resolveLines(
    EDGES_FILE,
    edgesLines.slice(-1)[0] === "" ? edgesLines.slice(0, -1) : edgesLines,
    opts,
    "edges",
  );
  stats.edgeConflicts = xRes.resolved + xRes.manualLeft;
  stats.resolvedEdges = xRes.resolved;
  stats.manualLeft += xRes.manualLeft;
  stats.warnings.push(...xRes.warnings);

  // ── Collect errors from resolution ────────────────────────────────────────
  const allErrors = [...eRes.errors, ...xRes.errors];

  // ── Summary ───────────────────────────────────────────────────────────────
  if (stats.entityConflicts === 0 && stats.edgeConflicts === 0) {
    console.log("No conflicts to resolve.");
    return 0;
  }

  // ── Write (or print) ──────────────────────────────────────────────────────
  if (opts.dryRun) {
    console.log(
      `Would resolve ${stats.resolvedEntities} entity conflicts and ` +
        `${stats.resolvedEdges} edge conflicts (--dry-run, no writes).`,
    );
    if (stats.manualLeft > 0) {
      console.log(`  ${stats.manualLeft} marked --manual or require explicit override`);
    }
    for (const e of allErrors) {
      console.error(`  ERROR  ${e}`);
    }
    return allErrors.length > 0 || stats.manualLeft > 0 ? 1 : 0;
  }

  // Sort only when NO manual conflict markers remain in the resolved content.
  // If manual blocks are present the JSON lines inside them must not be moved
  // — re-sorting a file with residual conflict markers would corrupt those
  // blocks (ADR-053 §3 step 3 — "re-sorts after resolution").
  let newEntitiesLines = eRes.newLines;
  if (eRes.resolved > 0 && eRes.manualLeft === 0) {
    newEntitiesLines = sortEntitiesNdjson(newEntitiesLines);
  }

  let newEdgesLines = xRes.newLines;
  if (xRes.resolved > 0 && xRes.manualLeft === 0) {
    newEdgesLines = sortEdgesNdjson(newEdgesLines);
  }

  // ── Atomic stage + validate + rename ──────────────────────────────────────
  // Stage resolved content to temp files on the SAME filesystem before
  // touching the originals. Only on successful validation do we atomically
  // replace the originals. A crash or validation failure between the two
  // file writes cannot leave the user with a mixed entity/edge state.

  let entitiesTmp: string | null = null;
  let edgesTmp: string | null = null;

  try {
    if (eRes.resolved + eRes.manualLeft > 0) {
      const writeText = newEntitiesLines.join("\n") +
        (newEntitiesLines.length > 0 ? "\n" : "");
      entitiesTmp = await writeStagedFile(entitiesPath, writeText);
    }
    if (xRes.resolved + xRes.manualLeft > 0) {
      const writeText = newEdgesLines.join("\n") +
        (newEdgesLines.length > 0 ? "\n" : "");
      edgesTmp = await writeStagedFile(edgesPath, writeText);
    }

    // ── Validate the staged files before committing ─────────────────────────
    // Validation needs the final file paths to exist. We temporarily rename
    // the originals aside and point the temp files to the final paths so
    // validate() reads the proposed content. If validation fails we roll back.
    if (stats.manualLeft === 0 && (entitiesTmp !== null || edgesTmp !== null)) {
      // Swap temps to final paths for validation.
      if (entitiesTmp !== null) await Deno.rename(entitiesTmp, entitiesPath);
      if (edgesTmp !== null) await Deno.rename(edgesTmp, edgesPath);
      // Mark as committed so the finally block skips cleanup.
      entitiesTmp = null;
      edgesTmp = null;

      const v = await validate(repoRoot);
      if (!v.valid) {
        printValidationResult(v);
        console.error(
          "\nResolution complete but validation failed. Fix issues before commit.",
        );
        // Originals have already been replaced with the resolved (but invalid)
        // content. The user must fix validation issues manually and re-run
        // 'khive kg validate'.
        return 1;
      }
    } else if (stats.manualLeft === 0) {
      // Nothing was written — no-op, but still run validate to confirm.
      const v = await validate(repoRoot);
      if (!v.valid) {
        printValidationResult(v);
        console.error(
          "\nResolution complete but validation failed. Fix issues before commit.",
        );
        return 1;
      }
    } else {
      // Manual conflicts remain — write the intermediate state with markers
      // still in place, then skip validation (it would fail on markers).
      if (entitiesTmp !== null) {
        await Deno.rename(entitiesTmp, entitiesPath);
        entitiesTmp = null;
      }
      if (edgesTmp !== null) {
        await Deno.rename(edgesTmp, edgesPath);
        edgesTmp = null;
      }
    }
  } finally {
    // Clean up any staged files that weren't atomically renamed.
    if (entitiesTmp !== null) await removeStagedFile(entitiesTmp);
    if (edgesTmp !== null) await removeStagedFile(edgesTmp);
  }

  // ── Print result ──────────────────────────────────────────────────────────
  console.log(
    `Resolved ${stats.resolvedEntities} entity conflicts, ` +
      `${stats.resolvedEdges} edge conflicts.`,
  );
  if (stats.manualLeft > 0) {
    console.log(
      `  ${stats.manualLeft} left as manual conflict markers (use --entity <id> --ours|--theirs to finish).`,
    );
  }
  for (const w of stats.warnings) {
    console.warn(`  WARN  ${w}`);
  }
  for (const e of allErrors) {
    console.error(`  ERROR  ${e}`);
  }

  if (allErrors.length > 0) {
    return 1;
  }

  if (stats.manualLeft > 0) {
    console.log(
      "Manual conflicts remain — fix them and re-run 'khive kg resolve' or 'khive kg validate'.",
    );
    return 1;
  }

  console.log("Validation: pass. Run 'git add' and 'git commit' to finalize.");
  return 0;
}
