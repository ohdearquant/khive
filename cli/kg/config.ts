/**
 * `khive kg config` — inspect and modify project configuration (ADR-057).
 *
 * Subcommands:
 *   khive kg config                              Show resolved config.
 *   khive kg config get <dotted.key>             Print a single value.
 *   khive kg config set <dotted.key> <val>       Write into `.khive/config.toml`.
 *   khive kg config set --global <key> <val>     Write into `~/.khive/config.toml`.
 *   khive kg config path                         Print the project config file path.
 *
 * Two-level resolution: built-in defaults < global (~/.khive/config.toml) < project.
 * `get` reflects the resolved value. `set` writes only into the project file.
 *
 * Key governance (ADR-057 §2):
 *   Project-writable keys: embed.model, embed.dimensions, embed.auto_embed,
 *     embed.batch_size, schema.strict
 *   Array keys (edit manually): embed.fields.include
 *   User-level keys (--global only): embed.device, auth.api_url
 *
 * Phase C1: get/show/path only. `set` performs a string-based TOML rewrite —
 * suitable for scalar keys (e.g. `embed.model`, `embed.batch_size`, `schema.strict`).
 * Nested table writes that don't already exist in the file print an error pointing
 * to manual editing.
 */

import { CONFIG_FILE, GLOBAL_CONFIG_FILE } from "../lib/paths.ts";
import { ALLOWED_DEVICES, type KhiveConfig, loadConfig } from "../lib/config.ts";

// ─── Key governance (ADR-057 §2) ──────────────────────────────────────────────

/**
 * Keys that may be written into the project config (`.khive/config.toml`).
 * These are committed to git and shared across all collaborators.
 *
 * Note: `embed.fields.include` is an array and is intentionally excluded
 * from this set. The scalar TOML rewriter cannot safely mutate array values.
 * To change embed.fields.include, edit `.khive/config.toml` directly.
 */
const PROJECT_KEYS: ReadonlySet<string> = new Set([
  "embed.model",
  "embed.dimensions",
  "embed.auto_embed",
  "embed.batch_size",
  "schema.strict",
]);

/**
 * Keys that are user-level only (`~/.khive/config.toml`). Writing these into
 * the project config would commit machine-specific settings to git.
 */
const USER_KEYS: ReadonlySet<string> = new Set([
  "embed.device",
  "auth.api_url",
]);

/**
 * All known keys (project + user). Anything outside this set is rejected.
 */
const ALL_KNOWN_KEYS: ReadonlySet<string> = new Set([
  ...PROJECT_KEYS,
  ...USER_KEYS,
]);

/**
 * Validate that `rawValue` is an acceptable scalar for `dotted` (ADR-057 §8).
 * Returns an error string on failure, or null if the value is valid.
 * Exported for testing.
 */
export function validateSetValue(dotted: string, rawValue: string): string | null {
  switch (dotted) {
    case "embed.dimensions":
    case "embed.batch_size": {
      // Must be a positive integer — no decimals, no negatives, no non-numeric.
      if (!/^\d+$/.test(rawValue)) {
        return `'${dotted}' must be a positive integer. Got: "${rawValue}"`;
      }
      const n = Number(rawValue);
      if (!Number.isInteger(n) || n <= 0) {
        return `'${dotted}' must be a positive integer. Got: "${rawValue}"`;
      }
      return null;
    }
    case "embed.auto_embed":
    case "schema.strict": {
      if (rawValue !== "true" && rawValue !== "false") {
        return `'${dotted}' must be a boolean (true or false). Got: "${rawValue}"`;
      }
      return null;
    }
    case "embed.model":
    case "auth.api_url": {
      if (rawValue.trim().length === 0) {
        return `'${dotted}' must be a non-empty string.`;
      }
      return null;
    }
    case "embed.device": {
      if (!ALLOWED_DEVICES.includes(rawValue)) {
        return (
          `'embed.device' must be one of: ${ALLOWED_DEVICES.join(", ")}. ` +
          `Got: "${rawValue}"`
        );
      }
      return null;
    }
    default:
      // Unknown keys are caught by checkSetKey before this is called.
      return null;
  }
}

/**
 * Pure validation for `config set` key routing (ADR-057 §2).
 * Returns an error string if the key/mode combination is invalid, or null if OK.
 * Exported for testing.
 */
export function checkSetKey(
  dotted: string,
  globalMode: boolean,
): string | null {
  if (!ALL_KNOWN_KEYS.has(dotted)) {
    const valid = [...PROJECT_KEYS, ...USER_KEYS].sort().join(", ");
    return `Unknown config key: '${dotted}'. Valid keys: ${valid}`;
  }
  if (!globalMode && USER_KEYS.has(dotted)) {
    return (
      `'${dotted}' is a user-level key and must not be committed to project config. ` +
      `Use 'khive kg config set --global ${dotted} <value>' to write ~/.khive/config.toml`
    );
  }
  return null;
}

function printHelp(): void {
  console.log(`Usage: khive kg config [subcommand] [args]

Subcommands:
  (none)                         Show the resolved configuration.
  get <key>                      Print a single value (dotted path).
  set <key> <value>              Write into .khive/config.toml (project keys only).
  set --global <key> <value>     Write into ~/.khive/config.toml (user-level keys).
  path                           Print the project config file path.

Project-level keys (committed to git):
  embed.model, embed.dimensions, embed.auto_embed, embed.batch_size,
  schema.strict
  (embed.fields.include is an array — edit .khive/config.toml manually)

User-level keys (--global, not committed):
  embed.device, auth.api_url

Examples:
  khive kg config
  khive kg config get embed.model
  khive kg config set embed.auto_embed false
  khive kg config set embed.batch_size 128
  khive kg config set --global embed.device metal`);
}

function pickByPath(
  config: KhiveConfig,
  dotted: string,
): unknown {
  const parts = dotted.split(".");
  let cur: unknown = config;
  for (const p of parts) {
    if (cur === null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[p];
  }
  return cur;
}

function formatValue(v: unknown): string {
  if (v === undefined) return "(unset)";
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return JSON.stringify(v);
}

function printConfig(config: KhiveConfig): void {
  console.log("[embed]");
  console.log(`model = "${config.embed.model}"`);
  console.log(`dimensions = ${config.embed.dimensions}`);
  console.log(`auto_embed = ${config.embed.auto_embed}`);
  console.log(`batch_size = ${config.embed.batch_size}`);
  console.log(`device = "${config.embed.device}"`);
  console.log("");
  console.log("[embed.fields]");
  console.log(`include = ${JSON.stringify(config.embed.fields.include)}`);
  console.log("");
  console.log("[schema]");
  console.log(`strict = ${config.schema.strict}`);
  console.log("");
  console.log("[auth]");
  console.log(`api_url = "${config.auth.api_url}"`);
}

// ─── Setter ───────────────────────────────────────────────────────────────────

interface ParsedDottedKey {
  table: string; // e.g. "embed" or "embed.fields"
  field: string; // e.g. "model"
}

function parseDottedKey(dotted: string): ParsedDottedKey | null {
  const parts = dotted.split(".");
  if (parts.length < 2) return null;
  const field = parts.pop()!;
  const table = parts.join(".");
  return { table, field };
}

function literalValue(raw: string): string {
  if (raw === "true" || raw === "false") return raw;
  if (/^-?\d+$/.test(raw)) return raw;
  if (/^-?\d+\.\d+$/.test(raw)) return raw;
  if (raw.startsWith("[") && raw.endsWith("]")) return raw;
  // Default: quote as a string.
  const escaped = raw.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
  return `"${escaped}"`;
}

/**
 * Write a single key into an existing TOML file.
 * For --global mode: creates the table if absent (append), or writes the
 * file from scratch if missing. For project mode: requires the table to
 * already exist.
 *
 * Exported for testing with explicit file paths.
 */
export async function writeConfigKey(
  configPath: string,
  dotted: string,
  rawValue: string,
  globalMode: boolean,
): Promise<string> {
  const parsed = parseDottedKey(dotted)!;
  let text: string;
  try {
    text = await Deno.readTextFile(configPath);
  } catch (err) {
    if (err instanceof Deno.errors.NotFound) {
      if (globalMode) {
        // Global config does not exist — create it with the table header.
        const literal2 = literalValue(rawValue);
        const newContent = `[${parsed.table}]\n${parsed.field} = ${literal2}\n`;
        await Deno.mkdir(
          configPath.replace(/\/[^/]+$/, ""),
          { recursive: true },
        );
        await Deno.writeTextFile(configPath, newContent);
        return `${dotted} = ${literal2}`;
      }
      throw new Error(`${configPath} not found. Run 'khive kg init' first.`);
    }
    throw err;
  }

  const tableHeader = `[${parsed.table}]`;
  if (!text.includes(tableHeader)) {
    if (globalMode) {
      // For --global, append the missing table + assignment rather than failing.
      // This supports partial configs (e.g. only [auth] present) and empty files.
      const literal2 = literalValue(rawValue);
      const sep = text.endsWith("\n") || text.length === 0 ? "" : "\n";
      const appendText = `${sep}[${parsed.table}]\n${parsed.field} = ${literal2}\n`;
      await Deno.writeTextFile(configPath, text + appendText);
      return `${dotted} = ${literal2}`;
    }
    throw new Error(
      `Table '${tableHeader}' is not present in ${configPath}. ` +
        `Edit the file manually to add it.`,
    );
  }

  const literal = literalValue(rawValue);
  const newLine = `${parsed.field} = ${literal}`;

  // Find table region: from the [table] header to the next [ header or EOF.
  const lines = text.split("\n");
  let tableStart = -1;
  let tableEnd = lines.length;
  for (let i = 0; i < lines.length; i++) {
    const trimmed = lines[i].trim();
    if (trimmed === tableHeader) {
      tableStart = i;
      continue;
    }
    if (tableStart !== -1 && trimmed.startsWith("[") && trimmed.endsWith("]")) {
      tableEnd = i;
      break;
    }
  }
  if (tableStart === -1) {
    throw new Error(`Could not locate '${tableHeader}' in ${configPath}`);
  }

  // Find existing assignment within the table region.
  const keyPattern = new RegExp(`^\\s*${parsed.field}\\s*=`);
  let updated = false;
  for (let i = tableStart + 1; i < tableEnd; i++) {
    const trimmed = lines[i].trim();
    if (trimmed.startsWith("#")) continue;
    if (keyPattern.test(lines[i])) {
      lines[i] = newLine;
      updated = true;
      break;
    }
  }
  if (!updated) {
    // Insert at the bottom of the table region (skipping trailing blank lines).
    let insertAt = tableEnd;
    while (insertAt > tableStart + 1 && lines[insertAt - 1].trim() === "") {
      insertAt--;
    }
    lines.splice(insertAt, 0, newLine);
  }

  await Deno.writeTextFile(configPath, lines.join("\n"));
  return `${dotted} = ${literal}`;
}

async function setKey(
  repoRoot: string,
  dotted: string,
  rawValue: string,
  globalMode: boolean,
): Promise<void> {
  // Key governance: reject unknown and misrouted keys.
  const keyError = checkSetKey(dotted, globalMode);
  if (keyError !== null) {
    console.error(keyError);
    Deno.exit(1);
  }
  if (!parseDottedKey(dotted)) {
    console.error(`Invalid key: '${dotted}'. Use dotted form, e.g. embed.model`);
    Deno.exit(1);
  }

  // Value validation: reject semantically invalid scalars before any disk write.
  const valueError = validateSetValue(dotted, rawValue);
  if (valueError !== null) {
    console.error(valueError);
    Deno.exit(1);
  }

  const configPath = globalMode ? GLOBAL_CONFIG_FILE : `${repoRoot}/${CONFIG_FILE}`;
  try {
    const msg = await writeConfigKey(configPath, dotted, rawValue, globalMode);
    console.log(msg);
  } catch (err) {
    console.error(err instanceof Error ? err.message : String(err));
    Deno.exit(1);
  }
}

// ─── Entry point ──────────────────────────────────────────────────────────────

export async function runConfig(
  repoRoot: string,
  args: string[],
): Promise<void> {
  const [subcommand, ...rest] = args;

  if (!subcommand) {
    const config = await loadConfig(repoRoot);
    printConfig(config);
    return;
  }

  if (subcommand === "--help" || subcommand === "-h") {
    printHelp();
    return;
  }

  if (subcommand === "path") {
    console.log(`${repoRoot}/${CONFIG_FILE}`);
    return;
  }

  if (subcommand === "get") {
    if (!rest[0]) {
      console.error("Missing key. Usage: khive kg config get <dotted.key>");
      Deno.exit(1);
    }
    const config = await loadConfig(repoRoot);
    const value = pickByPath(config, rest[0]);
    console.log(formatValue(value));
    return;
  }

  if (subcommand === "set") {
    // Support: khive kg config set --global <key> <value>
    const globalMode = rest[0] === "--global";
    const keyArg = globalMode ? rest[1] : rest[0];
    const valArg = globalMode ? rest[2] : rest[1];
    if (!keyArg || valArg === undefined) {
      console.error(
        "Usage: khive kg config set [--global] <dotted.key> <value>",
      );
      Deno.exit(1);
    }
    await setKey(repoRoot, keyArg, valArg, globalMode);
    return;
  }

  console.error(`Unknown config subcommand: '${subcommand}'`);
  console.error("Run 'khive kg config --help' for usage.");
  Deno.exit(1);
}
