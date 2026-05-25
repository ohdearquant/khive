/**
 * `khive kg import` — import a KgArchive JSON file into .khive/kg/ NDJSON files.
 *
 * Reads a KgArchive (format "khive-kg", version "0.1") produced by the Rust
 * runtime's `export_kg` or by `khive kg export --format archive`, validates
 * the candidate graph state against schema.yaml using the same pipeline as
 * `khive kg validate`, then durably publishes the canonical NDJSON files.
 *
 * Safety properties:
 *   - Writes to a temp directory first, fsyncs staged files and directories,
 *     then records a durable journal before performing any renames.
 *   - Refuses to overwrite existing .khive/kg/ NDJSON files unless --overwrite
 *     is passed.
 *   - On process crash at any point, the next `khive kg` command runs
 *     recoverImportJournal() which either rolls back (status=pending) or rolls
 *     forward (status=committed) to a consistent state.
 *   - Journal lives at .khive/.import-journal.json (gitignored by the default
 *     .khive/.gitignore allowlist).
 */

import { EDGES_FILE, ENTITIES_FILE, KG_DIR, SCHEMA_FILE } from "../lib/paths.ts";
import { DEFAULT_SCHEMA_YAML } from "../lib/schema.ts";
import { canonicalEdgeJson, canonicalEntityJson } from "../lib/canonical.ts";
import { readNdjson } from "../lib/ndjson.ts";
import { validate } from "./validate.ts";
import { adaptCsv } from "../lib/importers/csv.ts";
import { adaptJson } from "../lib/importers/json.ts";
import type { EdgeRecord, EntityRecord } from "../lib/importers/types.ts";

// ─── KgArchive types ──────────────────────────────────────────────────────────

interface KgArchiveEntity {
  id: string;
  kind: string;
  name: string;
  description?: string;
  properties?: Record<string, unknown>;
  tags?: string[];
  created_at?: string;
  updated_at?: string;
  [key: string]: unknown;
}

interface KgArchiveEdge {
  edge_id: string;
  source: string;
  target: string;
  relation: string;
  weight?: number;
  properties?: Record<string, unknown>;
  [key: string]: unknown;
}

interface KgArchive {
  format: string;
  version: string;
  namespace?: string;
  exported_at?: string;
  entities: KgArchiveEntity[];
  edges: KgArchiveEdge[];
}

// ─── Journal types ────────────────────────────────────────────────────────────

/** A single file swap descriptor: staged → live with optional .bak backup. */
interface JournalSwap {
  /** Absolute path to the staged (new) file. */
  staged: string;
  /** Absolute path to the live (destination) file. */
  live: string;
  /** Absolute path to the backup of the original live file, if one was created. */
  bak: string;
}

type JournalStatus = "pending" | "committed";

/**
 * Import journal written to .khive/.import-journal.json before any renames.
 *
 * status=pending  — journal written; renames may or may not have started.
 *                   Recovery: restore .bak → live, remove staging dir.
 * status=committed — all staged→live renames complete.
 *                   Recovery: remove .bak files and journal.
 */
interface ImportJournal {
  /** Absolute path to the temp staging directory. */
  staging_dir: string;
  /** Absolute path to the live .khive/kg/ directory. */
  target_dir: string;
  /** Ordered list of (staged, live, bak) triples describing each file swap. */
  files_to_swap: JournalSwap[];
  status: JournalStatus;
  timestamp: string;
}

// ─── Conflict resolution (for --on-conflict) ──────────────────────────────────

/**
 * Per-record conflict policy when importing into an existing KG (ADR-036 §5).
 *   error   — default; fail if any live files exist (file-level, not record-level)
 *   skip    — keep the existing record, ignore the incoming one
 *   replace — overwrite the existing record with the incoming one (legacy alias for update)
 *   merge   — deep-merge properties, union tags, preserve existing scalars (legacy alias for update)
 *   update  — patch existing record: deep-merge properties, union tags (ADR-036 canonical name)
 */
export type ConflictPolicy = "error" | "skip" | "replace" | "merge" | "update";

async function readExistingArchive(repoRoot: string): Promise<KgArchive> {
  const entities: KgArchiveEntity[] = [];
  const edges: KgArchiveEdge[] = [];
  for await (const { data } of readNdjson(`${repoRoot}/${ENTITIES_FILE}`)) {
    if (data) entities.push(data as unknown as KgArchiveEntity);
  }
  for await (const { data } of readNdjson(`${repoRoot}/${EDGES_FILE}`)) {
    if (data) edges.push(data as unknown as KgArchiveEdge);
  }
  return { format: "khive-kg", version: "0.1", entities, edges };
}

function deepMergeObjects(
  base: Record<string, unknown>,
  incoming: Record<string, unknown>,
): Record<string, unknown> {
  const result: Record<string, unknown> = { ...base };
  for (const [key, value] of Object.entries(incoming)) {
    if (
      typeof value === "object" &&
      value !== null &&
      !Array.isArray(value) &&
      typeof result[key] === "object" &&
      result[key] !== null &&
      !Array.isArray(result[key])
    ) {
      result[key] = deepMergeObjects(
        result[key] as Record<string, unknown>,
        value as Record<string, unknown>,
      );
    } else {
      result[key] = value;
    }
  }
  return result;
}

/**
 * Resolve an entity conflict according to policy.
 * Returns null to keep the existing record (skip), or the resolved entity.
 */
function mergeEntityConflict(
  existing: KgArchiveEntity,
  incoming: KgArchiveEntity,
  policy: ConflictPolicy,
): KgArchiveEntity | null {
  if (policy === "skip") return null;
  if (policy === "replace") return incoming;
  // update / merge: deep-merge properties, union+sort tags, prefer existing scalar fields.
  // ADR-036 §5 canonical name is "update"; "merge" is a legacy alias.
  const mergedProperties = deepMergeObjects(
    existing.properties ?? {},
    incoming.properties ?? {},
  );
  const tagSet = new Set([...(existing.tags ?? []), ...(incoming.tags ?? [])]);
  return {
    ...existing,
    properties: mergedProperties,
    tags: [...tagSet].sort(),
    updated_at: incoming.updated_at ?? existing.updated_at,
  };
}

/**
 * Resolve an edge conflict according to policy.
 * Returns null to keep the existing record (skip), or the resolved edge.
 */
function mergeEdgeConflict(
  existing: KgArchiveEdge,
  incoming: KgArchiveEdge,
  policy: ConflictPolicy,
): KgArchiveEdge | null {
  if (policy === "skip") return null;
  if (policy === "replace") return incoming;
  // update / merge: deep-merge properties, prefer incoming weight when present.
  const mergedProperties = deepMergeObjects(
    existing.properties ?? {},
    incoming.properties ?? {},
  );
  return {
    ...existing,
    properties: mergedProperties,
    weight: incoming.weight ?? existing.weight,
  };
}

/**
 * Build the candidate archive by merging existing and incoming archives
 * record-by-record according to the conflict policy.
 */
function buildCandidateArchive(
  existing: KgArchive,
  incoming: KgArchive,
  policy: ConflictPolicy,
): KgArchive {
  const entityMap = new Map(existing.entities.map((e) => [e.id, e]));
  const edgeMap = new Map(existing.edges.map((e) => [e.edge_id, e]));

  for (const incomingEntity of incoming.entities) {
    const existingEntity = entityMap.get(incomingEntity.id);
    if (!existingEntity) {
      entityMap.set(incomingEntity.id, incomingEntity);
    } else {
      const resolved = mergeEntityConflict(existingEntity, incomingEntity, policy);
      if (resolved !== null) entityMap.set(incomingEntity.id, resolved);
      // null → skip: existing record stays in map
    }
  }

  for (const incomingEdge of incoming.edges) {
    const existingEdge = edgeMap.get(incomingEdge.edge_id);
    if (!existingEdge) {
      edgeMap.set(incomingEdge.edge_id, incomingEdge);
    } else {
      const resolved = mergeEdgeConflict(existingEdge, incomingEdge, policy);
      if (resolved !== null) edgeMap.set(incomingEdge.edge_id, resolved);
    }
  }

  return {
    ...existing,
    entities: [...entityMap.values()],
    edges: [...edgeMap.values()],
  };
}

// ─── Sort helpers ─────────────────────────────────────────────────────────────

/** Sort key for an entity: its UUID string (lexicographic = UUID-ascending). */
function entitySortKey(e: KgArchiveEntity): string {
  return e.id.toLowerCase();
}

/** Sort key for an edge: composite key (source + target + relation). */
function edgeSortKey(edge: KgArchiveEdge): string {
  return `${edge.source.toLowerCase()}\x00${edge.target.toLowerCase()}\x00${edge.relation}`;
}

// ─── Basic field validation ───────────────────────────────────────────────────

function isUuid(value: unknown): value is string {
  return (
    typeof value === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value)
  );
}

function validateArchive(raw: unknown): KgArchive {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new Error("Archive must be a JSON object");
  }
  const obj = raw as Record<string, unknown>;

  if (obj["format"] !== "khive-kg") {
    throw new Error(
      `Unsupported archive format: expected "khive-kg", got ${JSON.stringify(obj["format"])}`,
    );
  }
  if (obj["version"] !== "0.1") {
    throw new Error(
      `Unsupported archive version: expected "0.1", got ${JSON.stringify(obj["version"])}`,
    );
  }
  if (!Array.isArray(obj["entities"])) {
    throw new Error('Archive must have an "entities" array');
  }
  if (!Array.isArray(obj["edges"])) {
    throw new Error('Archive must have an "edges" array');
  }

  // Validate each entity has required fields
  for (let i = 0; i < (obj["entities"] as unknown[]).length; i++) {
    const e = (obj["entities"] as unknown[])[i] as Record<string, unknown>;
    if (!isUuid(e["id"])) {
      throw new Error(`Entity[${i}] must have a UUID "id" field`);
    }
    if (typeof e["name"] !== "string" || e["name"].length === 0) {
      throw new Error(`Entity[${i}] must have a non-empty "name" field`);
    }
    if (typeof e["kind"] !== "string" || e["kind"].length === 0) {
      throw new Error(`Entity[${i}] must have a "kind" field`);
    }
  }

  // Validate each edge has required fields
  for (let i = 0; i < (obj["edges"] as unknown[]).length; i++) {
    const edge = (obj["edges"] as unknown[])[i] as Record<string, unknown>;
    if (!isUuid(edge["edge_id"])) {
      throw new Error(`Edge[${i}] must have a UUID "edge_id" field`);
    }
    if (typeof edge["source"] !== "string" || edge["source"].length === 0) {
      throw new Error(`Edge[${i}] must have a "source" field`);
    }
    if (typeof edge["target"] !== "string" || edge["target"].length === 0) {
      throw new Error(`Edge[${i}] must have a "target" field`);
    }
    if (typeof edge["relation"] !== "string" || edge["relation"].length === 0) {
      throw new Error(`Edge[${i}] must have a "relation" field`);
    }
  }

  return obj as unknown as KgArchive;
}

// ─── fsync helpers ────────────────────────────────────────────────────────────

/**
 * Write text to a file and fsync before closing, ensuring bytes reach stable
 * storage before this function returns.
 */
async function writeFileSync(path: string, content: string): Promise<void> {
  const f = await Deno.open(path, { write: true, create: true, truncate: true });
  try {
    const encoded = new TextEncoder().encode(content);
    let written = 0;
    while (written < encoded.length) {
      written += await f.write(encoded.subarray(written));
    }
    await f.sync();
  } finally {
    f.close();
  }
}

/**
 * fsync a directory entry, flushing directory metadata to stable storage.
 * This ensures that any renames whose targets live in the directory are
 * durable (POSIX requires a directory fsync after rename for crash consistency).
 *
 * On platforms where opening a directory for read is not supported, this
 * degrades gracefully to a no-op (the rename durability promise weakens but
 * no error is thrown).
 */
async function syncDir(dirPath: string): Promise<void> {
  try {
    const f = await Deno.open(dirPath, { read: true });
    try {
      await f.sync();
    } finally {
      f.close();
    }
  } catch {
    // Ignore: on platforms that do not support opening directories,
    // rename durability is best-effort.
  }
}

// ─── Journal helpers ──────────────────────────────────────────────────────────

/** Absolute path to the import journal for a given repo root. */
function journalPath(repoRoot: string): string {
  return `${repoRoot}/.khive/.import-journal.json`;
}

/**
 * Write the import journal with status=pending and fsync it to stable storage.
 *
 * The journal MUST be durable before any live-file renames begin.  If the
 * process crashes after this returns, recoverImportJournal() can safely
 * determine what state the filesystem is in and undo or complete the operation.
 */
async function writeJournal(repoRoot: string, journal: ImportJournal): Promise<void> {
  await Deno.mkdir(`${repoRoot}/.khive`, { recursive: true });
  await writeFileSync(journalPath(repoRoot), JSON.stringify(journal, null, 2));
  // fsync the .khive/ directory so the journal's directory entry is durable.
  await syncDir(`${repoRoot}/.khive`);
}

/**
 * Update the journal status to "committed" and fsync.
 *
 * Called after all staged→live renames succeed.  If the process crashes after
 * this point, recoverImportJournal() rolls forward (deletes .bak + journal).
 */
async function markJournalCommitted(repoRoot: string): Promise<void> {
  let journal: ImportJournal;
  try {
    const text = await Deno.readTextFile(journalPath(repoRoot));
    journal = JSON.parse(text) as ImportJournal;
  } catch {
    // Journal already gone or unreadable — nothing to update.
    return;
  }
  journal.status = "committed";
  await writeFileSync(journalPath(repoRoot), JSON.stringify(journal, null, 2));
  await syncDir(`${repoRoot}/.khive`);
}

/**
 * Recover from an interrupted import.
 *
 * Must be called at the start of every `khive kg` command so that any process
 * crash during a previous import is healed before new operations run.
 *
 * Recovery is idempotent: re-running after a partial recovery is safe.
 *
 *   status=pending:
 *     Renames may or may not have started.  For each swap, if the staged file
 *     still exists the rename did not happen — no action needed for that file.
 *     If the staged file is gone (rename happened), restore .bak → live.
 *     If no .bak exists for a file whose staged copy is gone, the live copy is
 *     already correct (no previous live existed; staged copy became the live).
 *     Remove the staging dir and the journal.
 *
 *   status=committed:
 *     All staged→live renames completed.  Delete any remaining .bak files and
 *     the journal.
 *
 * @param repoRoot  Absolute path to the repository root.
 * @returns         A description of the recovery action taken, or null if no
 *                  journal was found.
 */
export async function recoverImportJournal(
  repoRoot: string,
): Promise<"rolled_back" | "rolled_forward" | null> {
  let journal: ImportJournal;
  try {
    const text = await Deno.readTextFile(journalPath(repoRoot));
    journal = JSON.parse(text) as ImportJournal;
  } catch {
    // No journal found — clean state.
    return null;
  }

  if (journal.status === "committed") {
    // Roll forward: all renames completed; clean up .bak files and journal.
    for (const swap of journal.files_to_swap) {
      await Deno.remove(swap.bak).catch(() => {});
    }
    await Deno.remove(journalPath(repoRoot)).catch(() => {});
    return "rolled_forward";
  }

  // status === "pending": determine per-file what happened and undo.
  for (const swap of journal.files_to_swap) {
    let stagedExists = false;
    try {
      await Deno.stat(swap.staged);
      stagedExists = true;
    } catch {
      // staged file gone
    }

    if (stagedExists) {
      // Rename did not happen yet for this file.  Nothing to restore —
      // the live file is still the original (or did not exist to begin with).
      // If there is a .bak (from a prior swap that did run), restore it.
      // But if staged is present the rename for THIS file hasn't run, so no
      // .bak should exist from THIS swap.  A .bak might still exist from a
      // previous partially-completed swap of a different file.
      // We handle that conservatively: always attempt to restore .bak→live
      // for every swap, treating the presence of .bak as the source of truth.
    }

    // Attempt to restore .bak → live regardless of staged presence.
    // If .bak exists, it is the original and should be the live file.
    let bakExists = false;
    try {
      await Deno.stat(swap.bak);
      bakExists = true;
    } catch {
      // no .bak
    }
    if (bakExists) {
      // Remove whatever is currently at the live path (may be the new staged
      // content that was renamed into place).
      await Deno.remove(swap.live).catch(() => {});
      await Deno.rename(swap.bak, swap.live).catch(() => {});
    } else if (!stagedExists) {
      // Staged gone AND no .bak — the live file IS already the staged content
      // (rename happened) but there was no previous live to back up.
      // We cannot recover the original (there was none); the live file is fine.
    }
    // else: staged still present, no .bak — live is the original, nothing to do.
  }

  // Remove staging dir (may or may not exist).
  await Deno.remove(journal.staging_dir, { recursive: true }).catch(() => {});
  // Remove journal last so recovery is re-entrant if we crash during cleanup.
  await Deno.remove(journalPath(repoRoot)).catch(() => {});
  await syncDir(`${repoRoot}/.khive`);

  return "rolled_back";
}

// ─── Core implementation (throws on error — testable without Deno.exit) ──────

/**
 * Import a KgArchive from a file path into repoRoot's NDJSON files.
 *
 * Steps:
 *   1. Parse and structurally validate the archive.
 *   2. Sort entities (UUID-ascending) and edges (composite-key-ascending).
 *   3. Serialize to canonical NDJSON in a temp directory; fsync staged files.
 *   4. Run `validate()` against the temp directory to enforce closed kinds,
 *      closed relations, referential integrity, duplicate detection, and sort
 *      order.
 *   5. If validation passes:
 *      - Without --overwrite: error if .khive/kg/entities.ndjson or
 *        .khive/kg/edges.ndjson already exist.
 *      - Write a durable journal, perform atomic renames, fsync, mark committed.
 *   6. Clean up .bak files, staging dir, and journal.
 *
 * Throws with a descriptive message on any error.
 *
 * @param repoRoot    Absolute path to the repository root.
 * @param archivePath Path to the KgArchive JSON file to import.
 * @param options     Optional flags and test hooks.
 */
export async function importArchive(
  repoRoot: string,
  archivePath: string,
  options: {
    overwrite?: boolean;
    /**
     * Per-record conflict policy for entities and edges that already exist.
     * When set, the file-level overwrite check is bypassed.
     * Ignored when `overwrite` is also true (full replacement takes precedence).
     */
    onConflict?: ConflictPolicy;
    /**
     * @internal — in-process crash hook for caught-error recovery tests.
     *
     * Subprocess crash regression tests may still use KHIVE_TEST_CRASH_AFTER,
     * but only when KHIVE_DEV=1 is also present in the environment.
     */
    _afterFirstRename?: () => void | Promise<void>;
  } = {},
): Promise<void> {
  // ── 1. Read and parse archive ─────────────────────────────────────────────
  let raw: unknown;
  try {
    const text = await Deno.readTextFile(archivePath);
    raw = JSON.parse(text);
  } catch (err) {
    if (err instanceof Deno.errors.NotFound) {
      throw new Error(`archive file not found: ${archivePath}`);
    } else if (err instanceof SyntaxError) {
      throw new Error(`archive file is not valid JSON: ${(err as Error).message}`);
    } else {
      throw new Error(`Error reading archive: ${(err as Error).message}`);
    }
  }

  // ── 2. Validate archive structure ─────────────────────────────────────────
  const archive = validateArchive(raw);

  // ── 2b. Apply per-record conflict resolution ──────────────────────────────
  //
  // When --on-conflict is set (and not combined with --overwrite), read the
  // existing live files and merge records according to the policy before sorting
  // and staging the candidate.  This is a no-op if no live files exist yet.
  let candidateArchive = archive;
  if (options.onConflict && options.onConflict !== "error" && !options.overwrite) {
    const existing = await readExistingArchive(repoRoot);
    if (existing.entities.length > 0 || existing.edges.length > 0) {
      candidateArchive = buildCandidateArchive(existing, archive, options.onConflict);
    }
  }

  // ── 3. Sort entities and edges ────────────────────────────────────────────
  const sortedEntities = [...candidateArchive.entities].sort((a, b) =>
    entitySortKey(a).localeCompare(entitySortKey(b))
  );

  const sortedEdges = [...candidateArchive.edges].sort((a, b) =>
    edgeSortKey(a).localeCompare(edgeSortKey(b))
  );

  // ── 4. Write candidate NDJSON to temp directory and fsync ─────────────────
  // Use repoRoot as the parent for the temp dir so that all renames stay on the
  // same filesystem and Deno.rename() is guaranteed to be atomic (no EXDEV).
  await Deno.mkdir(`${repoRoot}/${KG_DIR}`, { recursive: true });
  const tmpDir = await Deno.makeTempDir({ dir: repoRoot, prefix: ".khive-import-tmp-" });
  const tmpKgDir = `${tmpDir}/${KG_DIR}`;
  await Deno.mkdir(tmpKgDir, { recursive: true });

  const tmpEntitiesPath = `${tmpDir}/${ENTITIES_FILE}`;
  const tmpEdgesPath = `${tmpDir}/${EDGES_FILE}`;

  const entitiesNdjson =
    sortedEntities.map((e) => canonicalEntityJson(e as Record<string, unknown>)).join("\n") +
    (sortedEntities.length > 0 ? "\n" : "");
  await writeFileSync(tmpEntitiesPath, entitiesNdjson);

  const edgesNdjson =
    sortedEdges.map((e) => canonicalEdgeJson(e as Record<string, unknown>)).join("\n") +
    (sortedEdges.length > 0 ? "\n" : "");
  await writeFileSync(tmpEdgesPath, edgesNdjson);

  // Provide schema.yaml for validate() to use:
  //   - Start with the built-in default (covers the closed ADR-001/ADR-002 sets).
  //   - Overwrite with the project schema if one exists (may have remotes etc.).
  const schemaDest = `${tmpDir}/${SCHEMA_FILE}`;
  await writeFileSync(schemaDest, DEFAULT_SCHEMA_YAML);
  const schemaSource = `${repoRoot}/${SCHEMA_FILE}`;
  try {
    const schemaText = await Deno.readTextFile(schemaSource);
    await writeFileSync(schemaDest, schemaText);
  } catch {
    // No project schema.yaml — use the default already written above.
  }

  // fsync the staging directory so the file entries are durable.
  await syncDir(tmpKgDir);
  await syncDir(tmpDir);

  // ── 5. Validate candidate state ───────────────────────────────────────────
  let validationPassed = false;
  try {
    const result = await validate(tmpDir);
    if (!result.valid) {
      const errorLines = result.errors
        .slice(0, 10)
        .map((e) => `  ${e.file}:${e.line}  ${e.message}`)
        .join("\n");
      const more = result.errors.length > 10 ? `\n  ... and ${result.errors.length - 10} more` : "";
      throw new Error(`Import rejected — validation failed:\n${errorLines}${more}`);
    }
    validationPassed = true;
  } finally {
    if (!validationPassed) {
      await Deno.remove(tmpDir, { recursive: true }).catch(() => {});
    }
  }

  // ── 6. Check overwrite policy ─────────────────────────────────────────────
  const destEntitiesPath = `${repoRoot}/${ENTITIES_FILE}`;
  const destEdgesPath = `${repoRoot}/${EDGES_FILE}`;

  // File-level overwrite guard: skip when --overwrite is set OR when
  // --on-conflict is set (per-record merging already handled conflicts above).
  if (!options.overwrite && !options.onConflict) {
    for (const path of [destEntitiesPath, destEdgesPath]) {
      try {
        await Deno.stat(path);
        // File exists — refuse without --overwrite or --on-conflict
        await Deno.remove(tmpDir, { recursive: true }).catch(() => {});
        throw new Error(
          `${path} already exists. Pass --overwrite to replace it, ` +
            `or --on-conflict <skip|replace|merge> for per-record handling.`,
        );
      } catch (err) {
        if (err instanceof Deno.errors.NotFound) {
          // Does not exist — fine to proceed
        } else {
          throw err;
        }
      }
    }
  }

  // ── 7. Durable atomic publish ─────────────────────────────────────────────
  //
  // Protocol (crash-safe):
  //
  //   (a) fsync staged files and staging directory (done above in step 4).
  //
  //   (b) Write a journal to .khive/.import-journal.json with status=pending,
  //       listing every (staged, live, bak) triple.  fsync the journal and its
  //       parent directory so it is durable before any rename starts.
  //
  //   (c) Backup phase: rename each live → .bak (atomic POSIX same-FS rename).
  //
  //   (d) Commit phase: rename each staged → live (atomic POSIX same-FS rename).
  //       After each rename, check KHIVE_TEST_CRASH_AFTER to simulate a crash
  //       for subprocess-crash regression tests.
  //
  //   (e) fsync the live .khive/kg/ directory (makes directory entries durable).
  //
  //   (f) Mark journal status=committed and fsync.
  //
  //   (g) Delete .bak files, staging dir, and journal.
  //
  // Recovery (recoverImportJournal, called by every kg command on startup):
  //   status=pending   → restore .bak → live, remove staging dir + journal.
  //   status=committed → remove .bak files + journal (roll forward).

  const destEntitiesBak = `${destEntitiesPath}.bak`;
  const destEdgesBak = `${destEdgesPath}.bak`;
  const kgDirPath = `${repoRoot}/${KG_DIR}`;

  // (b) Build the journal and write it durably before any renames.
  const journal: ImportJournal = {
    staging_dir: tmpDir,
    target_dir: kgDirPath,
    files_to_swap: [
      { staged: tmpEntitiesPath, live: destEntitiesPath, bak: destEntitiesBak },
      { staged: tmpEdgesPath, live: destEdgesPath, bak: destEdgesBak },
    ],
    status: "pending",
    timestamp: new Date().toISOString(),
  };
  await writeJournal(repoRoot, journal);

  // Dev-only crash hook for subprocess-crash regression tests.
  if (
    Deno.env.get("KHIVE_DEV") === "1" &&
    Deno.env.get("KHIVE_TEST_CRASH_AFTER") === "journal_written"
  ) {
    Deno.exit(42);
  }

  // (c) Backup phase: rename existing live files → .bak.
  let entitiesBakCreated = false;
  let edgesBakCreated = false;

  try {
    await Deno.rename(destEntitiesPath, destEntitiesBak);
    entitiesBakCreated = true;
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) {
      await Deno.remove(tmpDir, { recursive: true }).catch(() => {});
      await Deno.remove(journalPath(repoRoot)).catch(() => {});
      throw new Error(`Failed to back up ${destEntitiesPath}: ${(err as Error).message}`);
    }
    // No original — nothing to back up.
  }

  try {
    await Deno.rename(destEdgesPath, destEdgesBak);
    edgesBakCreated = true;
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) {
      // Restore entities.bak before giving up.
      if (entitiesBakCreated) {
        await Deno.rename(destEntitiesBak, destEntitiesPath).catch(() => {});
      }
      await Deno.remove(tmpDir, { recursive: true }).catch(() => {});
      await Deno.remove(journalPath(repoRoot)).catch(() => {});
      throw new Error(`Failed to back up ${destEdgesPath}: ${(err as Error).message}`);
    }
    // No original — nothing to back up.
  }

  // (d) Commit phase: rename staged → live.
  //
  // The entire commit phase is wrapped in a single try-catch so that any
  // exception (including those from _afterFirstRename or unexpected I/O errors)
  // triggers the same in-process rollback.  The journal (status=pending) also
  // covers the out-of-process crash case via recoverImportJournal.
  //
  // Tracks whether the first rename (entities) has completed so the rollback
  // knows whether to restore entities from .bak.
  let entitiesRenamed = false;
  try {
    await Deno.rename(tmpEntitiesPath, destEntitiesPath);
    entitiesRenamed = true;

    // In-process exception hook (preserved for caught-error recovery tests).
    if (options._afterFirstRename) await options._afterFirstRename();

    // Dev-only crash hook: crash after entities renamed but before edges renamed.
    // The journal (status=pending) + .bak files ensure recoverImportJournal
    // can deterministically roll back this state.
    if (
      Deno.env.get("KHIVE_DEV") === "1" &&
      Deno.env.get("KHIVE_TEST_CRASH_AFTER") === "first_rename"
    ) {
      Deno.exit(42);
    }

    await Deno.rename(tmpEdgesPath, destEdgesPath);
  } catch (err) {
    // Rollback: restore originals from .bak, remove staging dir and journal.
    //
    // If entities was successfully renamed (entitiesRenamed=true) but edges was
    // not, the live entities.ndjson now contains staged content — remove it and
    // restore from .bak.  If entities rename failed (entitiesRenamed=false),
    // the staged file is still in tmpDir and the live file was not touched.
    if (entitiesRenamed) {
      await Deno.remove(destEntitiesPath).catch(() => {});
      if (entitiesBakCreated) {
        await Deno.rename(destEntitiesBak, destEntitiesPath).catch(() => {});
      }
    } else {
      // entities rename failed: restore .bak if one exists (shouldn't happen
      // in practice since the entities rename itself is the first mutation).
      if (entitiesBakCreated) {
        await Deno.rename(destEntitiesBak, destEntitiesPath).catch(() => {});
      }
    }
    if (edgesBakCreated) {
      // edges was backed up but never renamed — restore from .bak.
      await Deno.rename(destEdgesBak, destEdgesPath).catch(() => {});
    }
    await Deno.remove(tmpDir, { recursive: true }).catch(() => {});
    await Deno.remove(journalPath(repoRoot)).catch(() => {});
    throw new Error(`Failed to publish NDJSON files: ${(err as Error).message}`);
  }

  // (e) fsync the live directory so the new directory entries are durable.
  await syncDir(kgDirPath);

  // (f) Mark journal committed and fsync.
  await markJournalCommitted(repoRoot);

  // (g) Clean up .bak files, staging dir, and journal.
  if (entitiesBakCreated) await Deno.remove(destEntitiesBak).catch(() => {});
  if (edgesBakCreated) await Deno.remove(destEdgesBak).catch(() => {});
  await Deno.remove(tmpDir, { recursive: true }).catch(() => {});
  await Deno.remove(journalPath(repoRoot)).catch(() => {});

  // ── 8. Report ─────────────────────────────────────────────────────────────
  console.log(
    `Imported ${sortedEntities.length} entities and ${sortedEdges.length} edges from ${archivePath}`,
  );
}

// ─── Format adapter helpers ───────────────────────────────────────────────────

/**
 * Detect format from a file path extension (ADR-036 §1 extension table).
 * Returns the format string or undefined when the extension is ambiguous.
 */
function detectFormat(filePath: string): string | undefined {
  const lower = filePath.toLowerCase();
  if (lower.endsWith(".ndjson")) return "ndjson";
  if (lower.endsWith(".csv")) return "csv";
  if (lower.endsWith(".tsv")) return "tsv";
  // .json is intentionally excluded: both KgArchive and generic JSON use .json.
  // Use --format json explicitly to invoke the JSON adapter; without the flag,
  // .json files fall through to the default (ndjson / archive path).
  return undefined;
}

/**
 * Convert adapter records (EntityRecord[] + EdgeRecord[]) into a KgArchive
 * so they can be passed to `importArchive` for durable, validated publish.
 */
function adapterResultToArchive(
  entities: EntityRecord[],
  edges: EdgeRecord[],
): KgArchive {
  const archiveEntities: KgArchiveEntity[] = entities.map((e) => ({
    id: e.id,
    kind: e.kind,
    name: e.name,
    description: e.description,
    properties: e.properties as Record<string, unknown>,
    tags: e.tags,
  }));
  const archiveEdges: KgArchiveEdge[] = edges.map((e) => ({
    edge_id: e.edge_id,
    source: e.source,
    target: e.target,
    relation: e.relation,
    weight: e.weight,
    properties: e.properties as Record<string, unknown>,
  }));
  return {
    format: "khive-kg",
    version: "0.1",
    entities: archiveEntities,
    edges: archiveEdges,
  };
}

/**
 * Import via a format adapter (CSV, TSV, JSON).
 *
 * Reads the source file, converts records using the appropriate adapter,
 * builds a KgArchive, then delegates to `importArchive` for durable publish.
 *
 * @param repoRoot    Repository root.
 * @param sourcePath  Path to the source file.
 * @param format      Normalized format name: "csv", "tsv", or "json".
 * @param defaultKind Default entity kind when source rows omit `kind`.
 * @param options     Import options forwarded to `importArchive`.
 */
async function importViaAdapter(
  repoRoot: string,
  sourcePath: string,
  format: string,
  defaultKind: string | undefined,
  options: {
    overwrite?: boolean;
    onConflict?: ConflictPolicy;
  } = {},
): Promise<void> {
  let text: string;
  try {
    text = await Deno.readTextFile(sourcePath);
  } catch (err) {
    if (err instanceof Deno.errors.NotFound) {
      throw new Error(`source file not found: ${sourcePath}`);
    }
    throw new Error(`Error reading source file: ${(err as Error).message}`);
  }

  let entities: EntityRecord[];
  let edges: EdgeRecord[];

  if (format === "csv" || format === "tsv") {
    const result = adaptCsv(text, {
      separator: format === "tsv" ? "\t" : ",",
      defaultKind,
    });
    entities = result.entities;
    edges = result.edges;
    if (result.warnings.length > 0) {
      for (const w of result.warnings) console.warn(`Warning: ${w}`);
    }
  } else if (format === "json") {
    const result = adaptJson(text, defaultKind);
    entities = result.entities;
    edges = result.edges;
    if (result.warnings.length > 0) {
      for (const w of result.warnings) console.warn(`Warning: ${w}`);
    }
  } else {
    throw new Error(
      `format '${format}' is not yet implemented.\n` +
        `Supported formats (P0): ndjson, csv, tsv, json.\n` +
        `See ADR-036 for the deferred format roadmap.`,
    );
  }

  const archive = adapterResultToArchive(entities, edges);

  // Write archive to a temp JSON file so importArchive can read it.
  const tmpFile = await Deno.makeTempFile({ prefix: ".khive-import-adapter-", suffix: ".json" });
  try {
    await Deno.writeTextFile(tmpFile, JSON.stringify(archive));
    await importArchive(repoRoot, tmpFile, options);
  } finally {
    await Deno.remove(tmpFile).catch(() => {});
  }

  console.log(
    `Imported ${entities.length} entities and ${edges.length} edges from ${sourcePath} (format: ${format})`,
  );
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

/**
 * `khive kg import [--format <fmt>] [--default-kind <kind>] [--overwrite]
 *                  [--on-conflict <error|skip|update|replace|merge>] <source-file>`
 *
 * Args:
 *   <source-file>               Path to the source file (required).
 *   --format <fmt>              Source format: ndjson (default), csv, tsv, json.
 *                               Inferred from file extension when absent (ADR-036 §1).
 *   --default-kind <kind>       Default entity kind when source rows omit `kind`.
 *   --overwrite                 Replace existing NDJSON files wholesale.
 *   --on-conflict <policy>      Per-record conflict: error | skip | update | replace | merge.
 *                               `update` is the ADR-036 canonical name; `replace` and `merge`
 *                               are legacy aliases retained for backward compatibility.
 *
 * Deferred flags (ADR-036 §9 — CLI rejects with "not yet implemented"):
 *   --mapping <path>            Column/field mapping file (P1).
 *   --schema-mode <mode>        Schema validation behavior (P1).
 *
 * Validates against schema.yaml before writing. Publishes durably via journal
 * protocol (crash-safe: recoverImportJournal handles process death mid-publish).
 * Exits 0 on success, 1 on error.
 */
export async function runImport(repoRoot: string, args: string[]): Promise<void> {
  // Reject deferred flags with a clear "not yet implemented" message (ADR-036 §9).
  if (args.includes("--mapping")) {
    console.error(
      "Error: --mapping is not yet implemented (deferred to P1 per ADR-036).",
    );
    Deno.exit(1);
  }
  if (args.includes("--schema-mode")) {
    console.error(
      "Error: --schema-mode is not yet implemented (deferred to P1 per ADR-036).",
    );
    Deno.exit(1);
  }

  const overwrite = args.includes("--overwrite");
  const isContinue = args.includes("--continue");

  // Parse --on-conflict <value> (ADR-036 canonical: error|skip|update; legacy: replace|merge)
  let onConflict: ConflictPolicy | undefined;
  const conflictIdx = args.indexOf("--on-conflict");
  if (conflictIdx !== -1) {
    const value = args[conflictIdx + 1];
    if (value === "skip" || value === "replace" || value === "merge" || value === "update") {
      onConflict = value;
    } else if (value === "error") {
      // "error" is the default; no-op but explicit.
      onConflict = undefined;
    } else {
      console.error(
        `Error: --on-conflict value must be 'error', 'skip', 'update', 'replace', or 'merge'; ` +
          `got '${value ?? "(missing)"}'`,
      );
      Deno.exit(1);
    }
  }

  // --continue is sugar for --on-conflict skip (ADR-036 §5).
  if (isContinue) {
    if (onConflict !== undefined) {
      console.error(
        "Error: --continue and --on-conflict cannot be combined (ADR-036 §5).",
      );
      Deno.exit(1);
    }
    onConflict = "skip";
  }

  // Parse --format <value>
  let explicitFormat: string | undefined;
  const formatIdx = args.indexOf("--format");
  if (formatIdx !== -1) {
    explicitFormat = args[formatIdx + 1];
    if (!explicitFormat || explicitFormat.startsWith("-")) {
      console.error("Error: --format requires a format argument");
      Deno.exit(1);
    }
  }

  // Parse --default-kind <value>
  let defaultKind: string | undefined;
  const kindIdx = args.indexOf("--default-kind");
  if (kindIdx !== -1) {
    defaultKind = args[kindIdx + 1];
    if (!defaultKind || defaultKind.startsWith("-")) {
      console.error("Error: --default-kind requires a kind argument");
      Deno.exit(1);
    }
  }

  // Positional arg: first non-flag argument, excluding known flag values.
  const flagsWithValues = new Set(["--on-conflict", "--format", "--default-kind"]);
  const sourcePath = args.find((a, i) => {
    if (a.startsWith("-")) return false;
    const prev = args[i - 1];
    return !flagsWithValues.has(prev);
  });
  if (!sourcePath) {
    console.error(
      "Usage: khive kg import [--format <fmt>] [--default-kind <kind>]\n" +
        "                       [--overwrite] [--on-conflict <error|skip|update>] <source-file>",
    );
    console.error("  <source-file>               Path to the source file (required)");
    console.error("  --format <fmt>              ndjson (default), csv, tsv, json");
    console.error("  --default-kind <kind>       Default entity kind when source omits kind");
    console.error("  --overwrite                 Replace existing NDJSON files without error");
    console.error(
      "  --on-conflict <policy>      Per-record: error (default) | skip | update | replace | merge",
    );
    Deno.exit(1);
  }

  // Resolve the format (explicit flag > file extension detection).
  const resolvedFormat = explicitFormat ?? detectFormat(sourcePath) ?? "ndjson";

  try {
    if (resolvedFormat === "ndjson") {
      // Native NDJSON/archive path: source must be a KgArchive JSON file.
      await importArchive(repoRoot, sourcePath, { overwrite, onConflict });
    } else {
      // Adapter path: CSV, TSV, or JSON format via format adapters (ADR-036).
      await importViaAdapter(repoRoot, sourcePath, resolvedFormat, defaultKind, {
        overwrite,
        onConflict,
      });
    }
  } catch (err) {
    console.error(`Error: ${(err as Error).message}`);
    Deno.exit(1);
  }
}
