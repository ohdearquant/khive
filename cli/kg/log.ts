/**
 * `khive kg log` — show KG change history.
 *
 * Runs `git log` filtered to .khive/kg/ NDJSON files and formats the output.
 */

import { exec } from "../lib/git.ts";
import { EDGES_FILE, ENTITIES_FILE, KG_DIR } from "../lib/paths.ts";

// ─── Types ────────────────────────────────────────────────────────────────────

interface LogOptions {
  limit: number;
  json: boolean;
  stat: boolean;
}

interface KgLogEntry {
  sha: string;
  author?: string;
  date?: string;
  subject: string;
  stats?: string[];
}

// ─── Arg parsing ──────────────────────────────────────────────────────────────

function parseLogArgs(args: string[]): LogOptions {
  const json = args.includes("--json");
  const stat = args.includes("--stat");

  let limit = 20;

  // -n <N> or --limit <N>
  const nIdx = args.findIndex((a) => a === "-n" || a === "--limit");
  if (nIdx !== -1 && args[nIdx + 1]) {
    const parsed = parseInt(args[nIdx + 1], 10);
    if (!isNaN(parsed) && parsed > 0) limit = parsed;
  }

  return { limit, json, stat };
}

// ─── Core logic ───────────────────────────────────────────────────────────────

export async function computeLog(repoRoot: string, args: string[]): Promise<KgLogEntry[]> {
  const options = parseLogArgs(args);

  // Tab-separated pretty format for reliable parsing.
  // Fields: SHA, ISO date, author, subject
  const format = "%H%x09%ad%x09%an%x09%s";

  const result = await exec([
    "git",
    "-C",
    repoRoot,
    "log",
    `--format=${format}`,
    "--date=iso-strict",
    `-n`,
    String(options.limit),
    "--",
    ENTITIES_FILE,
    EDGES_FILE,
  ]);

  if (result.code !== 0 || result.stdout.trim() === "") {
    return [];
  }

  const entries: KgLogEntry[] = [];
  for (const line of result.stdout.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const tabIdx = trimmed.indexOf("\t");
    if (tabIdx === -1) continue;
    const sha = trimmed.slice(0, tabIdx);
    const rest = trimmed.slice(tabIdx + 1);
    const parts = rest.split("\t");
    if (parts.length < 3) continue;
    const [date, author, ...subjectParts] = parts;
    entries.push({
      sha: sha.slice(0, 12),
      date,
      author,
      subject: subjectParts.join("\t"),
    });
  }

  if (options.stat) {
    for (const entry of entries) {
      const statResult = await exec([
        "git",
        "-C",
        repoRoot,
        "show",
        "--stat",
        "--oneline",
        entry.sha,
        "--",
        ENTITIES_FILE,
        EDGES_FILE,
      ]);
      if (statResult.code === 0) {
        // First line is "sha subject" — skip it; remainder are stat lines.
        const statLines = statResult.stdout.split("\n").slice(1).filter((l) => l.trim().length > 0);
        entry.stats = statLines;
      }
    }
  }

  return entries;
}

// ─── Formatting ───────────────────────────────────────────────────────────────

function formatLog(entries: KgLogEntry[], options: LogOptions): string {
  if (options.json) {
    return JSON.stringify(entries, null, 2);
  }

  if (entries.length === 0) {
    return "No KG commits found.";
  }

  const lines: string[] = [];
  for (const entry of entries) {
    lines.push(`commit ${entry.sha}`);
    if (entry.author) lines.push(`Author: ${entry.author}`);
    if (entry.date) lines.push(`Date:   ${entry.date}`);
    lines.push(`\n    ${entry.subject}`);
    if (entry.stats && entry.stats.length > 0) {
      lines.push("");
      for (const s of entry.stats) {
        lines.push(`  ${s}`);
      }
    }
    lines.push("");
  }

  return lines.join("\n").trimEnd();
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

export async function runLog(repoRoot: string, args: string[]): Promise<void> {
  if (args.includes("--help") || args.includes("-h")) {
    console.log(`Usage: khive kg log [-n <limit>] [--json] [--stat]

Show KG change history (commits that touched .khive/kg/ NDJSON files).

Flags:
  -n, --limit <N>   Maximum commits to show (default: 20)
  --json            Output entries as JSON
  --stat            Show file change statistics per commit`);
    return;
  }

  try {
    await Deno.stat(`${repoRoot}/${KG_DIR}`);
  } catch {
    console.log("KG not initialized. Run 'khive kg init' to start.");
    return;
  }

  const entries = await computeLog(repoRoot, args);
  console.log(formatLog(entries, parseLogArgs(args)));
}
