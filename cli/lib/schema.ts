/**
 * Schema loading and validation for .khive/kg/schema.yaml (ADR-048).
 *
 * schema.yaml is a simple, hand-maintained file.  We parse it with a
 * line-oriented approach rather than pulling in a full YAML library —
 * the file structure is regular enough that line-by-line parsing is
 * correct and dependency-free.
 *
 * Expected shape:
 *
 *   format_version: "1"
 *   entity_kinds:
 *     - concept
 *     - document
 *     ...
 *   edge_relations:
 *     - relation: contains
 *       description: "..."
 *     ...
 *   note_kinds:          # optional
 *     - observation
 *     ...
 *   remotes:             # optional
 *     upstream:
 *       url: "https://..."
 *       ref: "main"
 *   packs:               # optional
 *     - name: gtd
 *       version: "0.1"
 */

import { SCHEMA_FILE } from "./paths.ts";

// ─── Types ────────────────────────────────────────────────────────────────────

export interface EdgeRelationDef {
  relation: string;
  description?: string;
}

/** A remote KG reference as defined in ADR-048 §3. */
export interface RemoteDef {
  name: string;
  repo: string;
  path: string;
  commit: string;
}

export interface PackRef {
  name: string;
  version?: string;
}

export interface Schema {
  format_version: string;
  entity_kinds: string[];
  edge_relations: EdgeRelationDef[];
  note_kinds?: string[];
  /** Remotes are a list of {name, repo, path, commit} entries (ADR-048 §3). */
  remotes?: RemoteDef[];
  packs?: PackRef[];
}

export interface ValidationError {
  file: string;
  line: number;
  message: string;
}

// ─── Simple YAML parser ───────────────────────────────────────────────────────

/**
 * Minimal YAML parser for the schema.yaml subset we care about.
 *
 * Supports:
 *   - Top-level scalar keys: `key: value` or `key: "value"`
 *   - Sequence items: `  - value`
 *   - Mapping items under a sequence: `    key: value` after `  - relation: value`
 *   - Nested mappings: `remotes:` → `  name:` → `    url: value`
 *
 * Does NOT support: anchors, aliases, multi-line strings, explicit tags.
 */
function parseSchemaYaml(text: string): Schema {
  const lines = text.split("\n");
  const schema: Schema = {
    format_version: "",
    entity_kinds: [],
    edge_relations: [],
  };

  type ParseState =
    | "root"
    | "entity_kinds"
    | "edge_relations"
    | "note_kinds"
    | "remotes"
    | "remotes_entry"
    | "packs"
    | "packs_entry";

  let state: ParseState = "root";
  let currentEdge: Partial<EdgeRelationDef> | null = null;
  let currentPack: Partial<PackRef> | null = null;
  let currentRemote: Partial<RemoteDef> | null = null;

  function flushEdge() {
    if (currentEdge?.relation) {
      schema.edge_relations.push(currentEdge as EdgeRelationDef);
    }
    currentEdge = null;
  }

  function flushPack() {
    if (currentPack?.name) {
      schema.packs ??= [];
      schema.packs.push(currentPack as PackRef);
    }
    currentPack = null;
  }

  function flushRemote() {
    if (currentRemote?.name) {
      schema.remotes ??= [];
      schema.remotes.push(currentRemote as RemoteDef);
    }
    currentRemote = null;
  }

  for (const rawLine of lines) {
    const line = rawLine.trimEnd();
    if (line.trim() === "" || line.trim().startsWith("#")) continue;

    const indent = line.length - line.trimStart().length;
    const trimmed = line.trim();

    // Detect top-level section headers (indent 0, ends with ':')
    if (indent === 0) {
      flushEdge();
      flushPack();
      flushRemote();

      // scalar: `key: value`
      const scalarMatch = trimmed.match(/^(\w+):\s+(.+)$/);
      if (scalarMatch) {
        const key = scalarMatch[1];
        const val = scalarMatch[2].replace(/^["']|["']$/g, "").trim();
        if (key === "format_version") {
          schema.format_version = val;
        }
        state = "root";
        continue;
      }

      // section header: `key:`
      const sectionMatch = trimmed.match(/^(\w+):$/);
      if (sectionMatch) {
        const key = sectionMatch[1];
        switch (key) {
          case "entity_kinds":
            state = "entity_kinds";
            break;
          case "edge_relations":
            state = "edge_relations";
            break;
          case "note_kinds":
            schema.note_kinds ??= [];
            state = "note_kinds";
            break;
          case "remotes":
            schema.remotes ??= [];
            state = "remotes";
            break;
          case "packs":
            schema.packs ??= [];
            state = "packs";
            break;
          default:
            state = "root";
        }
        continue;
      }
    }

    // ── entity_kinds / note_kinds ─────────────────────────────────────────
    if (
      (state === "entity_kinds" || state === "note_kinds") &&
      indent === 2 &&
      trimmed.startsWith("- ")
    ) {
      const val = trimmed.slice(2).trim();
      if (state === "entity_kinds") {
        schema.entity_kinds.push(val);
      } else {
        schema.note_kinds!.push(val);
      }
      continue;
    }

    // ── edge_relations ────────────────────────────────────────────────────
    if (state === "edge_relations") {
      if (indent === 2 && trimmed.startsWith("- relation:")) {
        flushEdge();
        currentEdge = {
          relation: trimmed.replace(/^- relation:\s*/, "").replace(/^["']|["']$/g, "").trim(),
        };
        continue;
      }
      if (indent === 4 && currentEdge && trimmed.startsWith("description:")) {
        currentEdge.description = trimmed
          .replace(/^description:\s*/, "")
          .replace(/^["']|["']$/g, "")
          .trim();
        continue;
      }
      // simple list form: `  - contains`
      if (indent === 2 && trimmed.startsWith("- ") && !trimmed.includes(":")) {
        flushEdge();
        schema.edge_relations.push({ relation: trimmed.slice(2).trim() });
        continue;
      }
    }

    // ── remotes (ADR-048 §3: list of {name, repo, path, commit}) ─────────
    if (state === "remotes") {
      // New list entry: `  - name: lattice`
      if (indent === 2 && trimmed.startsWith("- name:")) {
        flushRemote();
        currentRemote = {
          name: trimmed.replace(/^- name:\s*/, "").replace(/^["']|["']$/g, "").trim(),
          repo: "",
          path: "",
          commit: "",
        };
        state = "remotes_entry";
        continue;
      }
    }
    if (state === "remotes_entry") {
      if (indent === 4) {
        const kvMatch = trimmed.match(/^(\w+):\s+(.+)$/);
        if (kvMatch && currentRemote) {
          const key = kvMatch[1];
          const val = kvMatch[2].replace(/^["']|["']$/g, "").trim();
          if (key === "repo") currentRemote.repo = val;
          if (key === "path") currentRemote.path = val;
          if (key === "commit") currentRemote.commit = val;
        }
        continue;
      }
      // Next remote list entry
      if (indent === 2 && trimmed.startsWith("- name:")) {
        flushRemote();
        currentRemote = {
          name: trimmed.replace(/^- name:\s*/, "").replace(/^["']|["']$/g, "").trim(),
          repo: "",
          path: "",
          commit: "",
        };
        continue;
      }
      if (indent === 0) {
        flushRemote();
        state = "root";
      }
    }

    // ── packs ─────────────────────────────────────────────────────────────
    if (state === "packs") {
      if (indent === 2 && trimmed.startsWith("- name:")) {
        flushPack();
        currentPack = {
          name: trimmed.replace(/^- name:\s*/, "").replace(/^["']|["']$/g, "").trim(),
        };
        continue;
      }
      if (indent === 2 && trimmed.startsWith("- ") && !trimmed.includes(":")) {
        // simple list: `  - gtd`
        flushPack();
        schema.packs!.push({ name: trimmed.slice(2).trim() });
        continue;
      }
      if (indent === 4 && currentPack) {
        const kvMatch = trimmed.match(/^(\w+):\s+(.+)$/);
        if (kvMatch) {
          const key = kvMatch[1];
          const val = kvMatch[2].replace(/^["']|["']$/g, "").trim();
          if (key === "version") currentPack.version = val;
        }
        continue;
      }
    }
  }

  flushEdge();
  flushPack();
  flushRemote();

  return schema;
}

// ─── Schema loading ───────────────────────────────────────────────────────────

/**
 * Load and parse schema.yaml from the repo root.
 * Throws if the file does not exist or cannot be parsed.
 */
export async function loadSchema(repoRoot: string): Promise<Schema> {
  const path = `${repoRoot}/${SCHEMA_FILE}`;
  const text = await Deno.readTextFile(path);
  return parseSchemaYaml(text);
}

// ─── Schema structural validation ────────────────────────────────────────────

/**
 * Validate a loaded Schema object for structural correctness.
 * Returns a list of ValidationErrors (empty = valid).
 */
export function validateSchema(schema: Schema): ValidationError[] {
  const errors: ValidationError[] = [];

  if (!schema.format_version) {
    errors.push({
      file: SCHEMA_FILE,
      line: 0,
      message: "Missing required field: format_version",
    });
  }

  if (!Array.isArray(schema.entity_kinds) || schema.entity_kinds.length === 0) {
    errors.push({
      file: SCHEMA_FILE,
      line: 0,
      message: "entity_kinds must be a non-empty list",
    });
  }

  if (!Array.isArray(schema.edge_relations) || schema.edge_relations.length === 0) {
    errors.push({
      file: SCHEMA_FILE,
      line: 0,
      message: "edge_relations must be a non-empty list",
    });
  }

  for (const rel of schema.edge_relations ?? []) {
    if (!rel.relation || typeof rel.relation !== "string") {
      errors.push({
        file: SCHEMA_FILE,
        line: 0,
        message: "Each edge_relations entry must have a non-empty 'relation' string",
      });
    }
  }

  return errors;
}
