// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

import { parseArgs } from "@std/cli/parse-args";
import { KhiveMcpClient } from "../mcp/client.ts";

const USAGE = `khive traverse — walk the graph from a starting node

Usage:
  khive traverse <ROOT-ID> [--depth N] [--direction out|in|both] [--namespace <NS>]

Arguments:
  ROOT-ID             UUID of the starting node

Options:
  --depth N           Maximum hop depth (default: 3)
  --direction D       out | in | both (default: out)
  --namespace <NS>    Namespace (defaults to server default)
  --json              Output raw JSON
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

/** A single path entry returned by the traverse tool. */
interface TraversalEntry {
  id?: string;
  full_id?: string;
  name?: string;
  kind?: string;
  relation?: string;
  depth?: number;
  source_id?: string;
}

/**
 * Render traversal results as an ASCII tree.
 *
 * The traverse tool returns a flat list of entries, each with a `depth` field
 * that indicates how many hops from the root. We sort by depth and use
 * indentation to visually represent the tree structure.
 *
 * Because the flat list does not carry explicit parent pointers for multi-hop
 * paths (only `source_id` which is the immediate source within the traversal),
 * we use source_id → id adjacency to build parent-child relationships and
 * render with tree connectors.
 */
function renderTree(entries: TraversalEntry[], rootId: string): void {
  if (entries.length === 0) {
    console.log("No nodes reachable from the root.");
    return;
  }

  // Build adjacency: source_id → [child entry, ...]
  const children = new Map<string, TraversalEntry[]>();
  // Also index entries by their id for root lookup
  const byId = new Map<string, TraversalEntry>();

  for (const e of entries) {
    const eid = e.id ?? e.full_id ?? "";
    byId.set(eid, e);
    if (e.source_id) {
      const list = children.get(e.source_id) ?? [];
      list.push(e);
      children.set(e.source_id, list);
    }
  }

  // Find root entries (depth 0 or no source_id, or source_id matches rootId arg)
  const roots = entries.filter((e) => e.depth === 0 || !e.source_id);

  function printNode(e: TraversalEntry, prefix: string, isLast: boolean): void {
    const connector = isLast ? "└── " : "├── ";
    const eid = e.id ?? e.full_id ?? "?";
    const shortId = eid.slice(0, 8);
    const label = e.name ? `${e.name} (${shortId})` : shortId;
    const rel = e.relation ? ` [${e.relation}]` : "";
    const kind = e.kind ? ` :${e.kind}` : "";
    console.log(`${prefix}${connector}${label}${kind}${rel}`);

    const childList = children.get(eid) ?? [];
    const childPrefix = prefix + (isLast ? "    " : "│   ");
    childList.forEach((child, i) => {
      printNode(child, childPrefix, i === childList.length - 1);
    });
  }

  // Print root header
  const shortRoot = rootId.slice(0, 8);
  console.log(`${shortRoot} (root)`);

  roots.forEach((r, i) => {
    printNode(r, "", i === roots.length - 1);
  });

  console.log(`\n${entries.length} node(s) reachable`);
}

export async function runTraverse(
  args: string[],
  globals: { namespace?: string; db?: string },
): Promise<number> {
  const flags = parseArgs(args, {
    string: ["direction", "namespace"],
    boolean: ["help", "json"],
    default: { depth: 3, direction: "out" },
    alias: { h: "help", ns: "namespace", d: "depth" },
  });

  if (flags.help) {
    console.log(USAGE);
    return 0;
  }

  const rootId = flags._[0] as string | undefined;
  if (!rootId) {
    console.error(
      "Error: ROOT-ID is required\nUsage: khive traverse <ROOT-ID> [--depth N]",
    );
    return 1;
  }

  const validDirections = new Set(["out", "in", "both"]);
  if (!validDirections.has(flags.direction)) {
    console.error(
      `Error: --direction must be one of: out | in | both (got "${flags.direction}")`,
    );
    return 1;
  }

  const namespace = flags.namespace ?? globals.namespace;
  const toolArgs: Record<string, unknown> = {
    roots: [String(rootId)],
    max_depth: Number(flags.depth),
    direction: flags.direction,
    include_roots: false,
  };
  if (namespace) toolArgs.namespace = namespace;

  const client = await KhiveMcpClient.connect();
  try {
    const result = await client.callTool("traverse", toolArgs);
    const text = extractText(result);
    if (flags.json) {
      console.log(text);
    } else {
      const entries: TraversalEntry[] = JSON.parse(text);
      renderTree(entries, String(rootId));
    }
    return 0;
  } catch (err) {
    console.error(`Error: ${(err as Error).message}`);
    return 1;
  } finally {
    await client.close();
  }
}
