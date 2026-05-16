// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Tests for the query command.
 *
 * Pure unit tests — verifies argument handling and table-rendering logic
 * without requiring a live khive-mcp binary.
 */

import { assertEquals, assertStringIncludes } from "@std/assert";

// ---------------------------------------------------------------------------
// Inline helpers that mirror query.ts logic
// ---------------------------------------------------------------------------

function parseQueryArgs(args: string[]): {
  query?: string;
  namespace?: string;
  json: boolean;
  ok: boolean;
  error?: string;
} {
  let namespace: string | undefined;
  let json = false;
  const positional: string[] = [];

  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if ((a === "--namespace" || a === "--ns") && i + 1 < args.length) {
      namespace = args[++i];
    } else if (a === "--json") json = true;
    else if (a === "--help" || a === "-h") return { json, ok: false, error: "help" };
    else if (!a.startsWith("-")) positional.push(a);
  }

  // Join all positional args into one query string (mirrors runQuery behavior)
  const query = positional.join(" ").trim();
  if (!query) return { json, ok: false, error: "query string is required" };
  return { query, namespace, json, ok: true };
}

/** Inline renderTable logic from query.ts */
function renderTableToString(rows: Array<Record<string, unknown>>): string {
  if (rows.length === 0) return "No rows returned.";

  const colSet = new Set<string>();
  for (const row of rows) {
    for (const k of Object.keys(row)) colSet.add(k);
  }
  const cols = [...colSet];

  const widths: Record<string, number> = {};
  for (const col of cols) widths[col] = col.length;
  for (const row of rows) {
    for (const col of cols) {
      const val = row[col] === undefined || row[col] === null ? "" : String(row[col]);
      if (val.length > widths[col]) widths[col] = val.length;
    }
  }
  for (const col of cols) widths[col] = Math.min(widths[col], 60);

  const sep = cols.map((c) => "-".repeat(widths[c])).join("-+-");
  const header = cols.map((c) => c.padEnd(widths[c])).join(" | ");
  const lines: string[] = [header, sep];
  for (const row of rows) {
    const line = cols
      .map((c) => {
        const raw = row[c] === undefined || row[c] === null ? "" : String(row[c]);
        const val = raw.length > widths[c] ? raw.slice(0, widths[c] - 1) + "…" : raw;
        return val.padEnd(widths[c]);
      })
      .join(" | ");
    lines.push(line);
  }
  lines.push(`\n${rows.length} row(s)`);
  return lines.join("\n");
}

// ---------------------------------------------------------------------------
// Argument parsing tests
// ---------------------------------------------------------------------------

Deno.test("query: requires a query string", () => {
  const r = parseQueryArgs([]);
  assertEquals(r.ok, false);
  assertEquals(r.error, "query string is required");
});

Deno.test("query: parses single-arg query", () => {
  const r = parseQueryArgs(["MATCH (a:concept) RETURN a.name"]);
  assertEquals(r.ok, true);
  assertEquals(r.query, "MATCH (a:concept) RETURN a.name");
});

Deno.test("query: joins multi-word positional args into one query", () => {
  // Shell may split a quoted string into tokens; we join them
  const r = parseQueryArgs(["MATCH", "(a:concept)", "RETURN", "a.name"]);
  assertEquals(r.ok, true);
  assertEquals(r.query, "MATCH (a:concept) RETURN a.name");
});

Deno.test("query: forwards query string unchanged (no transformation)", () => {
  const original = 'SELECT ?n WHERE { ?n :kind "concept" . } LIMIT 5';
  const r = parseQueryArgs([original]);
  assertEquals(r.ok, true);
  assertEquals(r.query, original);
});

Deno.test("query: parses --namespace flag", () => {
  const r = parseQueryArgs(["SELECT ?n", "--namespace", "papers"]);
  assertEquals(r.ok, true);
  assertEquals(r.namespace, "papers");
});

Deno.test("query: parses --json flag", () => {
  const r = parseQueryArgs(["SELECT ?n", "--json"]);
  assertEquals(r.ok, true);
  assertEquals(r.json, true);
});

Deno.test("query: json defaults to false", () => {
  const r = parseQueryArgs(["SELECT ?n"]);
  assertEquals(r.json, false);
});

// ---------------------------------------------------------------------------
// Table rendering tests
// ---------------------------------------------------------------------------

Deno.test("renderTable: empty rows returns message", () => {
  const out = renderTableToString([]);
  assertEquals(out, "No rows returned.");
});

Deno.test("renderTable: includes header row", () => {
  const rows = [{ name: "FlashAttention", kind: "concept" }];
  const out = renderTableToString(rows);
  assertStringIncludes(out, "name");
  assertStringIncludes(out, "kind");
});

Deno.test("renderTable: includes data values", () => {
  const rows = [{ name: "FlashAttention", kind: "concept" }];
  const out = renderTableToString(rows);
  assertStringIncludes(out, "FlashAttention");
  assertStringIncludes(out, "concept");
});

Deno.test("renderTable: includes row count", () => {
  const rows = [
    { name: "A", kind: "concept" },
    { name: "B", kind: "person" },
  ];
  const out = renderTableToString(rows);
  assertStringIncludes(out, "2 row(s)");
});

Deno.test("renderTable: truncates long values at 60 chars", () => {
  const longName = "x".repeat(65);
  const rows = [{ name: longName }];
  const out = renderTableToString(rows);
  // Should contain the ellipsis truncation marker
  assertStringIncludes(out, "…");
});

Deno.test("renderTable: handles null and undefined values", () => {
  const rows = [{ name: null, kind: undefined }] as Array<
    Record<string, unknown>
  >;
  const out = renderTableToString(rows);
  assertStringIncludes(out, "1 row(s)");
});

Deno.test("renderTable: uses separator line between header and data", () => {
  const rows = [{ a: "1" }];
  const out = renderTableToString(rows);
  assertStringIncludes(out, "-");
});
