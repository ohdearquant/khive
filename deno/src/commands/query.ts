// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

import { parseArgs } from "@std/cli/parse-args";
import { KhiveMcpClient } from "../mcp/client.ts";

const USAGE = `khive query — execute a GQL or SPARQL query

Usage:
  khive query "<query>" [--namespace <NS>] [--json]

Examples:
  khive query "MATCH (a:concept) RETURN a.name LIMIT 10"
  khive query "SELECT ?n WHERE { ?n :kind \\"concept\\" . }" --json

Options:
  --namespace <NS>    Namespace (defaults to server default)
  --json              Output raw JSON instead of a table
  --help, -h          Show this help
`;

/** Extract text content from an MCP CallToolResult. */
function extractText(result: unknown): string {
  const r = result as { content?: Array<{ type: string; text?: string }>; isError?: boolean };
  if (r.isError) {
    const errMsg = r.content?.find((c) => c.type === "text")?.text ?? "Unknown error";
    throw new Error(errMsg);
  }
  return r.content?.find((c) => c.type === "text")?.text ?? "";
}

/** Render an array of row objects as an ASCII table. */
function renderTable(rows: Array<Record<string, unknown>>): void {
  if (rows.length === 0) {
    console.log("No rows returned.");
    return;
  }

  // Collect all column names preserving first-seen order
  const colSet = new Set<string>();
  for (const row of rows) {
    for (const k of Object.keys(row)) colSet.add(k);
  }
  const cols = [...colSet];

  // Compute column widths (header vs data)
  const widths: Record<string, number> = {};
  for (const col of cols) {
    widths[col] = col.length;
  }
  for (const row of rows) {
    for (const col of cols) {
      const val = row[col] === undefined || row[col] === null ? "" : String(row[col]);
      if (val.length > widths[col]) widths[col] = val.length;
    }
  }
  // Cap width at 60 to keep the table readable
  for (const col of cols) {
    widths[col] = Math.min(widths[col], 60);
  }

  const sep = cols.map((c) => "-".repeat(widths[c])).join("-+-");
  const header = cols.map((c) => c.padEnd(widths[c])).join(" | ");
  console.log(header);
  console.log(sep);
  for (const row of rows) {
    const line = cols
      .map((c) => {
        const raw = row[c] === undefined || row[c] === null ? "" : String(row[c]);
        const val = raw.length > widths[c] ? raw.slice(0, widths[c] - 1) + "…" : raw;
        return val.padEnd(widths[c]);
      })
      .join(" | ");
    console.log(line);
  }
  console.log(`\n${rows.length} row(s)`);
}

export async function runQuery(
  args: string[],
  globals: { namespace?: string; db?: string },
): Promise<number> {
  const flags = parseArgs(args, {
    string: ["namespace"],
    boolean: ["help", "json"],
    alias: { h: "help", ns: "namespace" },
    stopEarly: false,
  });

  if (flags.help) {
    console.log(USAGE);
    return 0;
  }

  // Accept the query as the first positional arg OR joined if split by shell
  const queryStr = flags._.map(String).join(" ");
  if (!queryStr.trim()) {
    console.error(
      'Error: query string is required\nUsage: khive query "<query>"',
    );
    return 1;
  }

  const namespace = flags.namespace ?? globals.namespace;
  const toolArgs: Record<string, unknown> = { query: queryStr };
  if (namespace) toolArgs.namespace = namespace;

  const client = await KhiveMcpClient.connect();
  try {
    const result = await client.callTool("query", toolArgs);
    const text = extractText(result);
    if (flags.json) {
      console.log(text);
    } else {
      const rows: Array<Record<string, unknown>> = JSON.parse(text);
      renderTable(rows);
    }
    return 0;
  } catch (err) {
    console.error(`Error: ${(err as Error).message}`);
    return 1;
  } finally {
    await client.close();
  }
}
