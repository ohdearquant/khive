// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Unit tests for graph traversal routes.
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
// GET /api/traverse
// ---------------------------------------------------------------------------

Deno.test("GET /api/traverse — 400 when roots missing", async () => {
  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/traverse"));
  assertEquals(res.status, 400);
  assertEquals(mock.calls.length, 0);
});

Deno.test("GET /api/traverse?roots=abc — calls traverse", async () => {
  const mock = new MockMcpClient();
  mock.enqueue([{ id: "abc" }] as unknown as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/traverse?roots=abc"));
  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, `traverse(roots=["abc"], max_depth=3)`);
});

Deno.test("GET /api/traverse?roots=a,b&max_depth=2&relations=implements — full params", async () => {
  const mock = new MockMcpClient();
  mock.enqueue([{ id: "a" }, { id: "b" }] as unknown as McpResult);
  const app = makeApp(mock);

  await app.fetch(
    new Request(
      "http://localhost/api/traverse?roots=a,b&max_depth=2&relations=implements,extends",
    ),
  );
  assertEquals(
    mock.calls[0].ops,
    `traverse(roots=["a","b"], max_depth=2, relations=["implements","extends"])`,
  );
});

// ---------------------------------------------------------------------------
// GET /api/entities/:id/neighbors
// ---------------------------------------------------------------------------

Deno.test("GET /api/entities/:id/neighbors — calls neighbors", async () => {
  const mock = new MockMcpClient();
  mock.enqueue([{ id: "n1" }] as unknown as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/entities/node-1/neighbors"),
  );
  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, `neighbors(node_id="node-1")`);
});

Deno.test("GET /api/entities/:id/neighbors?direction=in&relations=contains — filters", async () => {
  const mock = new MockMcpClient();
  mock.enqueue([{ id: "n1" }] as unknown as McpResult);
  const app = makeApp(mock);

  await app.fetch(
    new Request(
      "http://localhost/api/entities/node-1/neighbors?direction=in&relations=contains",
    ),
  );
  assertEquals(
    mock.calls[0].ops,
    `neighbors(node_id="node-1", direction="in", relations=["contains"])`,
  );
});
