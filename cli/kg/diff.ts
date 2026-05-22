/**
 * `khive kg diff` — entity-level diff between two NDJSON states.
 *
 * Usage:
 *   khive kg diff [<base> [<head>]] [--json] [--name-only]
 *
 * With no args, diffs HEAD vs the working tree.
 * With one arg, diffs <base> vs the working tree.
 * With two args, diffs <base> vs <head>.
 */

import { exec } from "../lib/git.ts";
import { EDGES_FILE, ENTITIES_FILE, KG_DIR } from "../lib/paths.ts";

// ─── Types ────────────────────────────────────────────────────────────────────

type ChangeKind = "added" | "removed" | "modified";
type RecordKind = "entity" | "edge";

interface DiffOptions {
  base: string;
  head?: string;
  json: boolean;
  nameOnly: boolean;
}

interface KgRecordChange {
  kind: RecordKind;
  id: string;
  change: ChangeKind;
  fields: string[];
  before?: Record<string, unknown>;
  after?: Record<string, unknown>;
}

interface KgDiff {
  base: string;
  head: string;
  changes: KgRecordChange[];
}

// ─── Arg parsing ──────────────────────────────────────────────────────────────

function parseDiffArgs(args: string[]): DiffOptions {
  const json = args.includes("--json");
  const nameOnly = args.includes("--name-only");
  const positional = args.filter((a) => !a.startsWith("-"));
  const base = positional[0] ?? "HEAD";
  const head = positional[1]; // undefined → working tree
  return { base, head, json, nameOnly };
}

// ─── Core logic ───────────────────────────────────────────────────────────────

/**
 * Parse NDJSON text into a map keyed by the given field name.
 * Invalid and blank lines are silently skipped.
 */
export function parseRecordMap(
  text: string,
  keyField: "id" | "edge_id",
): Map<string, Record<string, unknown>> {
  const map = new Map<string, Record<string, unknown>>();
  for (const line of text.split("\n")) {
    const raw = line.trim();
    if (raw === "" || raw.startsWith("#")) continue;
    try {
      const data = JSON.parse(raw) as Record<string, unknown>;
      const key = data[keyField];
      if (typeof key === "string" && key.length > 0) {
        map.set(key, data);
      }
    } catch {
      // skip invalid JSON
    }
  }
  return map;
}

/**
 * Read the NDJSON content at a given path for a specific git ref.
 * If ref is undefined, reads directly from the filesystem (working tree).
 * Returns empty string when the ref or file does not exist.
 */
async function readRefText(
  repoRoot: string,
  ref: string | undefined,
  path: string,
): Promise<string> {
  if (ref === undefined) {
    try {
      return await Deno.readTextFile(`${repoRoot}/${path}`);
    } catch (err) {
      if (err instanceof Deno.errors.NotFound) return "";
      throw err;
    }
  }
  // Use -C so the command works regardless of the process CWD.
  const result = await exec(["git", "-C", repoRoot, "show", `${ref}:${path}`]);
  if (result.code !== 0) return "";
  return result.stdout;
}

/**
 * Diff two record maps and return the list of changes.
 */
export function diffMaps(
  baseMap: Map<string, Record<string, unknown>>,
  headMap: Map<string, Record<string, unknown>>,
  kind: RecordKind,
): KgRecordChange[] {
  const changes: KgRecordChange[] = [];

  for (const [id, before] of baseMap) {
    const after = headMap.get(id);
    if (!after) {
      changes.push({ kind, id, change: "removed", fields: [], before });
    } else {
      const modifiedFields: string[] = [];
      const allKeys = new Set([...Object.keys(before), ...Object.keys(after)]);
      for (const key of allKeys) {
        if (JSON.stringify(before[key]) !== JSON.stringify(after[key])) {
          modifiedFields.push(key);
        }
      }
      if (modifiedFields.length > 0) {
        changes.push({
          kind,
          id,
          change: "modified",
          fields: modifiedFields.sort(),
          before,
          after,
        });
      }
    }
  }

  for (const [id, after] of headMap) {
    if (!baseMap.has(id)) {
      changes.push({ kind, id, change: "added", fields: [], after });
    }
  }

  return changes;
}

export async function computeDiff(repoRoot: string, args: string[]): Promise<KgDiff> {
  const options = parseDiffArgs(args);

  const [baseEntitiesText, baseEdgesText, headEntitiesText, headEdgesText] = await Promise.all([
    readRefText(repoRoot, options.base, ENTITIES_FILE),
    readRefText(repoRoot, options.base, EDGES_FILE),
    readRefText(repoRoot, options.head, ENTITIES_FILE),
    readRefText(repoRoot, options.head, EDGES_FILE),
  ]);

  const entityChanges = diffMaps(
    parseRecordMap(baseEntitiesText, "id"),
    parseRecordMap(headEntitiesText, "id"),
    "entity",
  );
  const edgeChanges = diffMaps(
    parseRecordMap(baseEdgesText, "edge_id"),
    parseRecordMap(headEdgesText, "edge_id"),
    "edge",
  );

  return {
    base: options.base,
    head: options.head ?? "working tree",
    changes: [...entityChanges, ...edgeChanges],
  };
}

// ─── Formatting ───────────────────────────────────────────────────────────────

function formatDiff(diff: KgDiff, options: DiffOptions): string {
  if (options.json) {
    return JSON.stringify(diff, null, 2);
  }

  if (diff.changes.length === 0) {
    return `No KG changes between ${diff.base} and ${diff.head}`;
  }

  const lines: string[] = [`KG diff: ${diff.base} → ${diff.head}`];

  for (const change of diff.changes) {
    const sigil = change.change === "added" ? "+" : change.change === "removed" ? "-" : "~";
    if (options.nameOnly) {
      lines.push(`${sigil} ${change.kind}:${change.id}`);
    } else {
      const label = change.change === "modified"
        ? `${sigil} ${change.kind} ${change.id} (modified: ${change.fields.join(", ")})`
        : `${sigil} ${change.kind} ${change.id}`;
      lines.push(`  ${label}`);
      if (change.change === "added" && change.after) {
        const name = change.after["name"] as string | undefined;
        if (name) lines.push(`      name: ${name}`);
      }
    }
  }

  const added = diff.changes.filter((c) => c.change === "added").length;
  const removed = diff.changes.filter((c) => c.change === "removed").length;
  const modified = diff.changes.filter((c) => c.change === "modified").length;
  lines.push(`\n  ${added} added, ${removed} removed, ${modified} modified`);

  return lines.join("\n");
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

export async function runDiff(repoRoot: string, args: string[]): Promise<void> {
  if (args.includes("--help") || args.includes("-h")) {
    console.log(`Usage: khive kg diff [<base> [<head>]] [--json] [--name-only]

Show entity-level changes between two KG states.

Arguments:
  <base>        Git ref for the base state (default: HEAD)
  <head>        Git ref for the head state (default: working tree)

Flags:
  --json        Output changes as JSON
  --name-only   Show only IDs, not field details`);
    return;
  }

  try {
    await Deno.stat(`${repoRoot}/${KG_DIR}`);
  } catch {
    console.log("KG not initialized. Run 'khive kg init' to start.");
    return;
  }

  const options = parseDiffArgs(args);
  const diff = await computeDiff(repoRoot, args);
  console.log(formatDiff(diff, options));
}
