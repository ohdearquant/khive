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
 * Load the resolved configuration for a project.
 *
 * @param projectRoot  Absolute path to the repo root (from git rev-parse --show-toplevel).
 * @returns Merged KhiveConfig with defaults applied.
 */
export async function loadConfig(projectRoot: string): Promise<KhiveConfig> {
  const globalConfig = await readToml(GLOBAL_CONFIG_FILE);
  const projectConfig = await readToml(`${projectRoot}/.khive/config.toml`);
  // Project overrides global; global overrides defaults.
  return deepMerge(
    deepMerge(DEFAULTS as unknown as AnyObject, globalConfig as AnyObject),
    projectConfig as AnyObject,
  ) as unknown as KhiveConfig;
}
