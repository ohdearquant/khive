/**
 * Two-level TOML configuration loader (ADR-057 §1–§2).
 *
 * Resolution order: CLI flag > project config > global config > built-in default.
 * Project config (.khive/config.toml) beats global (~/.khive/config.toml).
 *
 * Phase C1 scope: this module defines and loads the config schema (correct scaffolding).
 * Config validation and `embed.auto_embed` behaviour are wired in Phase C2 when the
 * Rust embedding runtime is integrated.
 */

import { parse as parseTOML } from "@std/toml";
import { GLOBAL_CONFIG_FILE } from "./paths.ts";

// ─── Field validation ─────────────────────────────────────────────────────────

/**
 * Canonical top-level entity fields valid for embedding (ADR-057 §8 + ADR-001).
 * Any additional string is treated as a key in `entity.properties` and is
 * accepted at config validation time (the runtime reads it from `entity.properties`).
 * "kind" is reserved — it is a closed-taxonomy discriminant, not an embeddable field.
 */
export const ALLOWED_FIELDS: ReadonlyArray<string> = ["name", "description"];

/**
 * Field names that are structurally reserved and must not appear in
 * embed.fields.include (ADR-057 §8, ADR-001).
 */
const RESERVED_FIELDS: ReadonlySet<string> = new Set(["kind"]);

/**
 * Validate `embed.fields.include` against the allowed field set (ADR-057 §8).
 *
 * Rules:
 *   - Must be a non-empty array.
 *   - Every element must be a non-empty string.
 *   - No duplicates.
 *   - "kind" is reserved and must not be used (ADR-001 closed taxonomy).
 *   - "name" and "description" are canonical top-level fields.
 *   - Any other string is treated as an entity.properties key and is accepted
 *     (the runtime reads it from `entity.properties` at embed time).
 *
 * Returns a human-readable error message on failure, or null on success.
 */
export function validateEmbedFields(fields: string[]): string | null {
  if (!Array.isArray(fields) || fields.length === 0) {
    return `embed.fields.include must be a non-empty array. ` +
      `Top-level fields: ${ALLOWED_FIELDS.join(", ")}; ` +
      `any entity.properties key is also accepted.`;
  }
  const seen = new Set<string>();
  for (const f of fields) {
    if (typeof f !== "string" || f.length === 0) {
      return `embed.fields.include entries must be non-empty strings. Got: ${JSON.stringify(f)}`;
    }
    if (seen.has(f)) {
      return `embed.fields.include contains duplicate field: "${f}"`;
    }
    seen.add(f);
    if (RESERVED_FIELDS.has(f)) {
      return `embed.fields.include: "${f}" is reserved and cannot be embedded ` +
        `(it is a closed-taxonomy discriminant, not a text field).`;
    }
  }
  return null;
}

export interface EmbedFieldsConfig {
  include: string[];
}

export interface EmbedConfig {
  model: string;
  dimensions: number;
  auto_embed: boolean;
  batch_size: number;
  device: string;
  fields: EmbedFieldsConfig;
}

export interface SchemaConfig {
  strict: boolean;
}

export interface AuthConfig {
  api_url: string;
}

export interface KhiveConfig {
  embed: EmbedConfig;
  schema: SchemaConfig;
  auth: AuthConfig;
}

/**
 * Valid inference device values (ADR-057 §8 + §2 user-level key contract).
 * embed.device must be one of these three strings when set.
 */
export const ALLOWED_DEVICES: ReadonlyArray<string> = ["metal", "cuda", "cpu"];

// Built-in defaults (ADR-057 §2).
const DEFAULTS: KhiveConfig = {
  embed: {
    model: "mE5-small",
    dimensions: 384,
    auto_embed: true,
    batch_size: 64,
    device: "cpu",
    fields: { include: ["name", "description"] },
  },
  schema: { strict: true },
  // TODO: replace this placeholder with the real auth endpoint before commercial auth ships.
  auth: { api_url: "https://api.khive.ai" },
};

type AnyObject = Record<string, unknown>;

/**
 * Deep-merge two plain objects. `override` wins on scalar conflicts.
 * Arrays are replaced (not concatenated) — matching TOML semantics.
 */
function deepMerge(base: AnyObject, override: AnyObject): AnyObject {
  const result: AnyObject = { ...base };
  for (const [key, value] of Object.entries(override)) {
    if (
      value !== null &&
      typeof value === "object" &&
      !Array.isArray(value) &&
      typeof result[key] === "object" &&
      result[key] !== null &&
      !Array.isArray(result[key])
    ) {
      result[key] = deepMerge(
        result[key] as AnyObject,
        value as AnyObject,
      );
    } else if (value !== undefined) {
      result[key] = value;
    }
  }
  return result;
}

/**
 * Read and parse a TOML file. Returns an empty object if the file does not exist.
 * Throws on malformed TOML (parse errors include the file path).
 */
async function readToml(filePath: string): Promise<Partial<KhiveConfig>> {
  try {
    const text = await Deno.readTextFile(filePath);
    return parseTOML(text) as Partial<KhiveConfig>;
  } catch (err) {
    if (err instanceof Deno.errors.NotFound) return {};
    if (err instanceof Error) {
      throw new Error(`ERROR: ${filePath}: ${err.message}`);
    }
    throw err;
  }
}

/**
 * Validate a merged KhiveConfig. Throws with a descriptive message on any
 * violation (ADR-057 §8).
 */
export function validateConfig(config: KhiveConfig): void {
  const { embed } = config;

  if (typeof embed.model !== "string" || embed.model.length === 0) {
    throw new Error("embed.model must be a non-empty string");
  }
  if (!Number.isInteger(embed.dimensions) || embed.dimensions <= 0) {
    throw new Error("embed.dimensions must be a positive integer");
  }
  if (!Number.isInteger(embed.batch_size) || embed.batch_size <= 0) {
    throw new Error("embed.batch_size must be a positive integer");
  }
  if (typeof embed.auto_embed !== "boolean") {
    throw new Error("embed.auto_embed must be a boolean");
  }
  if (typeof config.schema.strict !== "boolean") {
    throw new Error("schema.strict must be a boolean");
  }

  // Validate embed.device (ADR-057 §8 + §2 user-level key contract).
  if (embed.device !== undefined && embed.device !== null) {
    if (!ALLOWED_DEVICES.includes(embed.device)) {
      throw new Error(
        `embed.device must be one of: ${ALLOWED_DEVICES.join(", ")}. ` +
          `Got: "${embed.device}"`,
      );
    }
  }

  // Validate embed.fields.include (ADR-057 §8).
  const fieldsError = validateEmbedFields(embed.fields?.include ?? []);
  if (fieldsError !== null) {
    throw new Error(fieldsError);
  }
}

/**
 * Keys that may only appear in the global config (~/.khive/config.toml).
 * Writing them into the project config commits machine-specific hardware
 * preferences to git, breaking collaborators (ADR-057 §2, §8).
 */
const USER_ONLY_KEYS: ReadonlySet<string> = new Set([
  "embed.device",
  "auth.api_url",
]);

/**
 * Reject user-only keys present in the project TOML fragment before merge
 * (ADR-057 §2, §8).  Throws with a clear message so users notice immediately.
 */
function rejectUserOnlyKeys(projectFragment: Partial<KhiveConfig>): void {
  const rawEmbed = (projectFragment as AnyObject)["embed"];
  const rawAuth = (projectFragment as AnyObject)["auth"];

  if (rawEmbed !== undefined) {
    if (typeof rawEmbed !== "object" || rawEmbed === null) {
      throw new Error(
        `embed must be a TOML table in project config (.khive/config.toml); ` +
          `got ${typeof rawEmbed}.`,
      );
    }
    if ("device" in (rawEmbed as AnyObject)) {
      throw new Error(
        `embed.device cannot be set in project config (.khive/config.toml) — ` +
          `it encodes a machine-local hardware preference and must not be ` +
          `committed to git. Move it to ~/.khive/config.toml instead.`,
      );
    }
  }
  if (rawAuth !== undefined) {
    if (typeof rawAuth !== "object" || rawAuth === null) {
      throw new Error(
        `auth must be a TOML table in project config (.khive/config.toml); ` +
          `got ${typeof rawAuth}.`,
      );
    }
    if ("api_url" in (rawAuth as AnyObject)) {
      throw new Error(
        `auth.api_url cannot be set in project config (.khive/config.toml) — ` +
          `it is a user-level key and must not be committed to git. ` +
          `Move it to ~/.khive/config.toml instead.`,
      );
    }
  }
}

// Re-export for use in cli/kg/config.ts governance layer.
export { USER_ONLY_KEYS };

/**
 * Load the resolved configuration for a project.
 *
 * @param projectRoot  Absolute path to the repo root (from git rev-parse --show-toplevel).
 * @returns Merged KhiveConfig with defaults applied.
 */
export async function loadConfig(projectRoot: string): Promise<KhiveConfig> {
  const globalConfig = await readToml(GLOBAL_CONFIG_FILE);
  const projectConfig = await readToml(`${projectRoot}/.khive/config.toml`);
  // Reject user-only keys before merge so the error is clear and immediate.
  rejectUserOnlyKeys(projectConfig);
  // Project overrides global; global overrides defaults.
  const merged = deepMerge(
    deepMerge(DEFAULTS as unknown as AnyObject, globalConfig as AnyObject),
    projectConfig as AnyObject,
  ) as unknown as KhiveConfig;
  validateConfig(merged);
  return merged;
}
