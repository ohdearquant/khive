// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Unit tests for edge routes.
 */

import { assertEquals } from "@std/assert";
import { createApp } from "../server.ts";
import type { McpClient, McpResult } from "../mcp/client.ts";

class MockMcpClient implements McpClient {
  calls: Array<{ ops: string }> = [];
  responses: McpResult[] = [];

  enqueue(r: McpResult): void {
    this.responses.push(r);
  }

  request(ops: string): Promise<McpResult> {
    this.calls.push({ ops });
    const r = this.responses.shift();
    if (!r) throw new Error(`no mock response queued for ops: ${ops}`);
    return Promise.resolve(r);
  }

  close(): Promise<void> {
    return Promise.resolve();
  }
}

function makeApp(mock: McpClient) {
  return createApp(mock);
}

// ---------------------------------------------------------------------------
// GET /api/edges
// ---------------------------------------------------------------------------

Deno.test("GET /api/edges — list edges", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ items: [] } as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/edges"));
  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, `list(kind="edge")`);
});

Deno.test("GET /api/edges?source_id=abc&relation=implements — filters", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ items: [] } as McpResult);
  const app = makeApp(mock);

  await app.fetch(
    new Request("http://localhost/api/edges?source_id=abc&relation=implements"),
  );
  assertEquals(
    mock.calls[0].ops,
    `list(kind="edge", source_id="abc", relation="implements")`,
  );
});

// ---------------------------------------------------------------------------
// POST /api/edges
// ---------------------------------------------------------------------------

Deno.test("POST /api/edges — creates edge", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ id: "edge-1", source_id: "a", target_id: "b" } as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/edges", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ source_id: "a", target_id: "b", relation: "implements" }),
    }),
  );

  assertEquals(res.status, 201);
  assertEquals(
    mock.calls[0].ops,
    `link(source_id="a", target_id="b", relation="implements")`,
  );
});

Deno.test("POST /api/edges — 400 when fields missing", async () => {
  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/edges", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ source_id: "a" }),
    }),
  );

  assertEquals(res.status, 400);
  assertEquals(mock.calls.length, 0);
});

Deno.test("POST /api/edges — with weight", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ id: "edge-2" } as McpResult);
  const app = makeApp(mock);

  await app.fetch(
    new Request("http://localhost/api/edges", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ source_id: "a", target_id: "b", relation: "extends", weight: 0.9 }),
    }),
  );

  assertEquals(
    mock.calls[0].ops,
    `link(source_id="a", target_id="b", relation="extends", weight=0.9)`,
  );
});

// ---------------------------------------------------------------------------
// DELETE /api/edges/:id
// ---------------------------------------------------------------------------

Deno.test("DELETE /api/edges/:id — deletes edge", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ deleted: true } as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/edges/edge-1", { method: "DELETE" }),
  );

  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, `delete(id="edge-1")`);
});
