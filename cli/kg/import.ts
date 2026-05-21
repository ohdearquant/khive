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
import { canonicalEdgeJson, canonicalEntityJson } from "../lib/canonical.ts";
import { validate } from "./validate.ts";

// Default schema.yaml content (matches init.ts's DEFAULT_SCHEMA_YAML).
// Used when the repo has no schema.yaml yet so validate() has the closed sets.
const DEFAULT_SCHEMA_YAML = `\
format_version: "1.0.0"
entity_kinds:
  - concept
  - document
  - dataset
  - project
  - person
  - org
edge_relations:
  - relation: contains
    category: structure
  - relation: part_of
    category: structure
  - relation: instance_of
    category: structure
  - relation: extends
    category: derivation
  - relation: variant_of
    category: derivation
  - relation: introduced_by
    category: derivation
  - relation: supersedes
    category: derivation
  - relation: depends_on
    category: dependency
  - relation: enables
    category: dependency
  - relation: implements
    category: implementation
  - relation: competes_with
    category: lateral
  - relation: composed_with
    category: lateral
  - relation: annotates
    category: annotation
note_kinds:
  - observation
  - insight
  - question
  - decision
  - reference
`;

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
     * @internal — test-only env-var crash point name.
     *
     * When the KHIVE_TEST_CRASH_AFTER environment variable equals one of these
     * values, the subprocess exits with code 42 at that point in the publish
     * sequence.  This is used by subprocess-crash regression tests.
     *
     * Values:
     *   "journal_written"  — after journal flushed, before any renames
     *   "first_rename"     — after entities.ndjson renamed into place, before edges
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

  // ── 3. Sort entities and edges ────────────────────────────────────────────
  const sortedEntities = [...archive.entities].sort((a, b) =>
    entitySortKey(a).localeCompare(entitySortKey(b))
  );

  const sortedEdges = [...archive.edges].sort((a, b) =>
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

  if (!options.overwrite) {
    for (const path of [destEntitiesPath, destEdgesPath]) {
      try {
        await Deno.stat(path);
        // File exists — refuse without --overwrite
        await Deno.remove(tmpDir, { recursive: true }).catch(() => {});
        throw new Error(
          `${path} already exists. Pass --overwrite to replace it.`,
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

  // Env-var crash hook for subprocess-crash regression tests.
  if (Deno.env.get("KHIVE_TEST_CRASH_AFTER") === "journal_written") {
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

    // Env-var crash hook: crash after entities renamed but before edges renamed.
    // The journal (status=pending) + .bak files ensure recoverImportJournal
    // can deterministically roll back this state.
    if (Deno.env.get("KHIVE_TEST_CRASH_AFTER") === "first_rename") {
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

// ─── CLI entry point ──────────────────────────────────────────────────────────

/**
 * `khive kg import [--overwrite] <archive-file>` command.
 *
 * Args:
 *   <archive-file>  Path to a KgArchive JSON file (required positional argument).
 *   --overwrite     Replace existing .khive/kg/ NDJSON files without error.
 *
 * Validates against schema.yaml before writing. Publishes durably via journal
 * protocol (crash-safe: recoverImportJournal handles process death mid-publish).
 * Exits 0 on success, 1 on error.
 */
export async function runImport(repoRoot: string, args: string[]): Promise<void> {
  const overwrite = args.includes("--overwrite");
  const archivePath = args.find((a) => !a.startsWith("-"));
  if (!archivePath) {
    console.error("Usage: khive kg import [--overwrite] <archive-file>");
    console.error("  <archive-file>  Path to a KgArchive JSON file (required)");
    console.error("  --overwrite     Replace existing NDJSON files without error");
    Deno.exit(1);
  }

  try {
    await importArchive(repoRoot, archivePath, { overwrite });
  } catch (err) {
    console.error(`Error: ${(err as Error).message}`);
    Deno.exit(1);
  }
}
