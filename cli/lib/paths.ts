/**
 * Canonical path constants for the .khive/kg/ layout (ADR-048).
 * All paths are relative to the repo root.
 */

export const KG_DIR = ".khive/kg";
export const ENTITIES_FILE = ".khive/kg/entities.ndjson";
export const EDGES_FILE = ".khive/kg/edges.ndjson";
export const SCHEMA_FILE = ".khive/kg/schema.yaml";
export const MIGRATIONS_DIR = ".khive/kg/migrations";
export const REMOTE_CACHE_DIR = ".khive/kg/.remote-cache";

export const CONFIG_FILE = ".khive/config.toml";
export const SETTINGS_FILE = ".khive/settings.json";

export const STATE_DIR = ".khive/state";
export const WORKING_DB = ".khive/state/working.db";

const HOME_DIR = Deno.env.get("HOME") ?? Deno.env.get("USERPROFILE") ?? "/tmp";
export const GLOBAL_CONFIG_DIR = `${HOME_DIR}/.khive`;
export const GLOBAL_CONFIG_FILE = `${GLOBAL_CONFIG_DIR}/config.toml`;
export const GLOBAL_AUTH_FILE = `${GLOBAL_CONFIG_DIR}/auth.json`;

/**
 * Ensure `.khive/state/` exists under repoRoot.
 *
 * Called by sync, status, commit, and any command that needs working.db.
 * This way `git clone` + `khive kg sync` works without requiring `khive kg init`.
 */
export async function ensureStateDir(repoRoot: string): Promise<void> {
  await Deno.mkdir(`${repoRoot}/${STATE_DIR}`, { recursive: true });
}
