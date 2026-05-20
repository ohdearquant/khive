// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Unit tests for search routes.
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

Deno.test("GET /api/search — 400 when query missing", async () => {
  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/search"));
  assertEquals(res.status, 400);
  const body = await res.json();
  assertEquals(body.error.code, "BAD_REQUEST");
  assertEquals(mock.calls.length, 0);
});

Deno.test("GET /api/search?query=attention — calls search verb", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ items: [] } as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/search?query=attention"));
  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, `search(query="attention", limit=20)`);
});

Deno.test("GET /api/search?query=x&kind=entity&limit=5 — passes filters", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ items: [] } as McpResult);
  const app = makeApp(mock);

  await app.fetch(new Request("http://localhost/api/search?query=x&kind=entity&limit=5"));
  assertEquals(mock.calls[0].ops, `search(query="x", kind="entity", limit=5)`);
});
