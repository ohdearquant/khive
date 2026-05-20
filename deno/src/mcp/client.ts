// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * MCP client wrapper for the khive HTTP gateway.
 *
 * Spawns `khive-mcp` as a child process and communicates over stdio MCP
 * using the single `request` tool (ADR-027). Every HTTP route translates
 * its params to a DSL ops string and calls `request(ops=...)`.
 *
 * The `McpClient` interface is the single seam between the HTTP layer and
 * the Rust runtime — all route handlers depend on this interface, not the
 * concrete class, enabling test injection.
 */

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

// ---------------------------------------------------------------------------
// Public interface (injectable for tests)
// ---------------------------------------------------------------------------

/** Result returned by the single `request` MCP tool. */
export interface McpResult {
  /** Per-op results for batch operations. */
  results?: Array<{
    ok: boolean;
    tool?: string;
    result?: unknown;
    error?: string;
  }>;
  /** Aggregate summary for batch operations. */
  summary?: {
    total: number;
    succeeded: number;
    failed: number;
  };
  /** Raw data payload for single-op calls. */
  [key: string]: unknown;
}

/** Minimal interface every route handler depends on. */
export interface McpClient {
  request(ops: string): Promise<McpResult>;
  close(): Promise<void>;
}

// ---------------------------------------------------------------------------
// Concrete implementation
// ---------------------------------------------------------------------------

/**
 * Wraps the MCP SDK Client and exposes `request(ops)` — the single-tool
 * surface defined in ADR-027. Spawned once at server start; shared across
 * all route handlers via Hono context.
 */
export class KhiveMcpClient implements McpClient {
  private client: Client;
  private transport: StdioClientTransport;

  private constructor(client: Client, transport: StdioClientTransport) {
    this.client = client;
    this.transport = transport;
  }

  /**
   * Connect to a `khive-mcp` process.
   *
   * @param command Full command string, e.g. `"khive-mcp --pack kg,gtd"`.
   *   Defaults to `KHIVE_MCP_BIN` env var, then `"khive-mcp"` on PATH.
   */
  static async connect(command?: string): Promise<KhiveMcpClient> {
    const cmd = command ??
      Deno.env.get("KHIVE_MCP_BIN") ??
      Deno.env.get("KHIVE_MCP_COMMAND") ??
      "khive-mcp";

    // Build pack flags from KHIVE_PACKS env if set
    const packs = Deno.env.get("KHIVE_PACKS");
    const packArgs = packs ? packs.split(",").flatMap((p) => ["--pack", p.trim()]) : [];

    const [bin, ...cmdArgs] = cmd.split(" ");
    const args = [...cmdArgs, ...packArgs];

    const transport = new StdioClientTransport({ command: bin, args });
    const client = new Client(
      { name: "khive-server", version: "0.1.0" },
      { capabilities: {} },
    );

    await client.connect(transport);
    return new KhiveMcpClient(client, transport);
  }

  /**
   * Call the `request` tool with a DSL ops string (ADR-027).
   *
   * The MCP SDK returns a CallToolResult:
   *   `{ content: [{ type: "text", text: "<json>" }], isError?: boolean }`
   *
   * We unwrap the text payload and parse the JSON.
   */
  async request(ops: string): Promise<McpResult> {
    const raw = await this.client.callTool({
      name: "request",
      arguments: { ops },
    });

    const result = raw as {
      content?: Array<{ type: string; text?: string }>;
      isError?: boolean;
    };

    if (result.isError) {
      const msg = result.content?.find((c) => c.type === "text")?.text ?? "unknown MCP error";
      throw new McpTransportError(msg);
    }

    const text = result.content?.find((c) => c.type === "text")?.text ?? "{}";
    try {
      return JSON.parse(text) as McpResult;
    } catch {
      throw new McpTransportError(`invalid JSON from khive-mcp: ${text}`);
    }
  }

  async close(): Promise<void> {
    await this.client.close();
  }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/** Thrown when the MCP transport layer fails (not a verb-level error). */
export class McpTransportError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "McpTransportError";
  }
}

// ---------------------------------------------------------------------------
// Helpers for unwrapping single-op results
// ---------------------------------------------------------------------------

/**
 * Extract the result payload from a single-op `request` call.
 *
 * The `request` tool returns either:
 *   - Single op:   `{ ok: true, result: <data> }` or `{ ok: false, error: "..." }`
 *   - Batch:       `{ results: [...], summary: {...} }`
 *
 * For single-op calls we unwrap one level.
 */
export function extractSingleResult(raw: McpResult): unknown {
  // Batch shape
  if (raw.results) {
    const first = raw.results[0];
    if (!first) throw new McpTransportError("empty results array");
    if (!first.ok) throw new VerseError(first.error ?? "verb error");
    return first.result;
  }
  // Single-op shape
  if (raw.ok === false) {
    throw new VerseError((raw.error as string) ?? "verb error");
  }
  return raw.result ?? raw;
}

/** Thrown when a verb returns `ok: false` (business-level error from Rust). */
export class VerseError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "VerseError";
  }
}
