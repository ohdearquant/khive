/**
 * CSV / TSV adapter (ADR-036 §2 — P0 format).
 *
 * Parses a CSV file into entity + edge records. Auto-detects whether the file
 * is an entity list or an edge list from the presence of `source` and `target`
 * columns. Without a mapping file:
 *   - file with source + target columns → edges
 *   - otherwise                          → entities (name required)
 *
 * Mapping files (ADR-036 §2 P0) are a future extension; this Phase E1 adapter
 * accepts only auto-detected schemas.
 *
 * Fatal errors (throw): empty/no-header CSV, missing required column, missing
 * required field in any row. Parse errors are never silently promoted to empty
 * results — the caller must handle them atomically.
 */

import type { EdgeRecord, EntityRecord } from "./types.ts";
import { randomUuid } from "./util.ts";

export interface CsvParseOptions {
  /** Field separator. Defaults to comma; pass "\t" for TSV. */
  separator?: string;
  /** Default `kind` if rows omit the kind column. */
  defaultKind?: string;
}

interface CsvRow {
  values: string[];
  line: number;
}

interface CsvFile {
  headers: string[];
  rows: CsvRow[];
}

function parseCsvText(text: string, sep: string): CsvFile {
  const lines = text.split(/\r?\n/);
  // Locate first non-empty line for headers.
  let headerIdx = -1;
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].trim().length > 0) {
      headerIdx = i;
      break;
    }
  }
  if (headerIdx < 0) return { headers: [], rows: [] };

  const headers = parseCsvLine(lines[headerIdx], sep).map((s) => s.trim());
  const rows: CsvRow[] = [];
  for (let i = headerIdx + 1; i < lines.length; i++) {
    const raw = lines[i];
    if (raw.trim().length === 0) continue;
    const values = parseCsvLine(raw, sep);
    // Reject rows whose field count does not match the header count (arity check).
    // This catches quoting bugs (e.g. unbalanced quotes causing a comma inside a
    // quoted field to split into extra fields) before they silently corrupt data.
    if (values.length !== headers.length) {
      throw new Error(
        `row ${
          i + 1
        }: field count mismatch — expected ${headers.length} fields, got ${values.length} ` +
          `(check for unbalanced quotes or extra commas)`,
      );
    }
    rows.push({ values, line: i + 1 });
  }
  return { headers, rows };
}

/**
 * RFC4180-style CSV line parser. Handles quoted values containing the separator
 * or embedded quotes ("" → "). Does not parse multi-line quoted values; CSV
 * sources with embedded newlines must be pre-flattened.
 *
 * Quoted fields: a `"` that appears at the start of a field (optionally preceded
 * by whitespace that is stripped before quoting begins) opens a quoted value.
 * Whitespace before the opening quote is discarded per common spreadsheet
 * export behaviour. Unbalanced quotes (quote mode still active at end of line)
 * are treated as a closing quote at the field boundary (lenient mode).
 */
function parseCsvLine(line: string, sep: string): string[] {
  const out: string[] = [];
  let i = 0;
  let cur = "";
  let inQuotes = false;
  while (i < line.length) {
    const c = line[i];
    if (inQuotes) {
      if (c === '"' && line[i + 1] === '"') {
        // Escaped double-quote inside quoted value.
        cur += '"';
        i += 2;
      } else if (c === '"') {
        // Closing quote.
        inQuotes = false;
        i++;
      } else {
        cur += c;
        i++;
      }
    } else {
      if (c === '"' && cur.trim() === "") {
        // Opening quote: strip any leading whitespace that preceded it and enter
        // quoted mode. cur.trim()==='' ensures we haven't started accumulating
        // a non-whitespace value yet.
        cur = "";
        inQuotes = true;
        i++;
      } else if (c === sep) {
        out.push(cur);
        cur = "";
        i++;
      } else {
        cur += c;
        i++;
      }
    }
  }
  // If still in quote mode at end of line, treat end-of-line as closing quote
  // (lenient: don't reject the entire import for an unbalanced quote in one field).
  out.push(cur);
  return out;
}

/** Lower-cased index map for column lookup. */
function buildIndex(headers: string[]): Map<string, number> {
  const m = new Map<string, number>();
  for (let i = 0; i < headers.length; i++) {
    m.set(headers[i].toLowerCase(), i);
  }
  return m;
}

function getCol(row: CsvRow, idx: number | undefined): string {
  if (idx === undefined) return "";
  return row.values[idx] ?? "";
}

// ─── Edge detection ──────────────────────────────────────────────────────────

function isEdgeFile(idx: Map<string, number>): boolean {
  return idx.has("source") && idx.has("target");
}

// ─── Public adapter ─────────────────────────────────────────────────────────

export interface CsvImportResult {
  entities: EntityRecord[];
  edges: EdgeRecord[];
  warnings: string[];
}

/**
 * Parse CSV/TSV text into entity/edge records.
 *
 * Throws on structural failures (empty file, missing required columns, missing
 * required fields on any row). These are never silently turned into warnings
 * because the caller must atomically reject the import on any such failure.
 */
export function adaptCsv(
  text: string,
  opts: CsvParseOptions = {},
): CsvImportResult {
  const sep = opts.separator ?? ",";
  const file = parseCsvText(text, sep);
  const idx = buildIndex(file.headers);

  if (file.headers.length === 0) {
    throw new Error("CSV has no header row");
  }

  if (isEdgeFile(idx)) {
    return adaptEdges(file, idx, opts);
  }
  return adaptEntities(file, idx, opts);
}

function adaptEntities(
  file: CsvFile,
  idx: Map<string, number>,
  opts: CsvParseOptions,
): CsvImportResult {
  const idCol = idx.get("id");
  const nameCol = idx.get("name");
  const kindCol = idx.get("kind");
  const descCol = idx.get("description");

  if (nameCol === undefined) {
    throw new Error("entity CSV missing required column 'name'");
  }

  const reserved = new Set([
    "id",
    "name",
    "kind",
    "description",
    "tags",
  ]);

  const entities: EntityRecord[] = [];
  for (const row of file.rows) {
    let id = getCol(row, idCol).trim();
    if (!id) id = randomUuid();

    const name = getCol(row, nameCol).trim();
    if (!name) {
      throw new Error(`row ${row.line}: empty name`);
    }

    let kind = getCol(row, kindCol).trim();
    if (!kind) {
      if (opts.defaultKind) kind = opts.defaultKind;
      else {
        throw new Error(
          `row ${row.line}: missing kind and no --default-kind specified`,
        );
      }
    }

    const description = descCol !== undefined ? getCol(row, descCol).trim() : undefined;
    const properties: Record<string, unknown> = {};

    // All other columns → properties (description stays top-level per ADR-048).
    for (let c = 0; c < file.headers.length; c++) {
      const header = file.headers[c].toLowerCase();
      if (reserved.has(header)) continue;
      const v = (row.values[c] ?? "").trim();
      if (v === "") continue;
      properties[file.headers[c]] = v;
    }

    const record: EntityRecord = { id, name, kind, properties };
    if (description) record.description = description;
    entities.push(record);
  }
  return { entities, edges: [], warnings: [] };
}

function adaptEdges(
  file: CsvFile,
  idx: Map<string, number>,
  _opts: CsvParseOptions,
): CsvImportResult {
  const sourceCol = idx.get("source");
  const targetCol = idx.get("target");
  const relCol = idx.get("relation");
  const weightCol = idx.get("weight");
  const edgeIdCol = idx.get("edge_id");

  if (relCol === undefined) {
    throw new Error("edge CSV missing required column 'relation'");
  }

  const reserved = new Set([
    "edge_id",
    "source",
    "target",
    "relation",
    "weight",
  ]);

  const edges: EdgeRecord[] = [];
  for (const row of file.rows) {
    const source = getCol(row, sourceCol).trim();
    const target = getCol(row, targetCol).trim();
    const relation = getCol(row, relCol).trim();
    if (!source || !target || !relation) {
      throw new Error(
        `row ${row.line}: missing source/target/relation`,
      );
    }
    let edge_id = getCol(row, edgeIdCol).trim();
    if (!edge_id) edge_id = randomUuid();
    let weight = 0.7;
    if (weightCol !== undefined) {
      const w = parseFloat(getCol(row, weightCol));
      if (!Number.isNaN(w)) weight = w;
    }

    const properties: Record<string, unknown> = {};
    for (let c = 0; c < file.headers.length; c++) {
      const header = file.headers[c].toLowerCase();
      if (reserved.has(header)) continue;
      const v = (row.values[c] ?? "").trim();
      if (v === "") continue;
      properties[file.headers[c]] = v;
    }

    edges.push({ edge_id, source, target, relation, weight, properties });
  }
  return { entities: [], edges, warnings: [] };
}
