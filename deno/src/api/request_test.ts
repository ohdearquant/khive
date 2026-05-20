// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Unit tests for the raw DSL passthrough route.
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

Deno.test("POST /api/request — forwards ops verbatim", async () => {
  const mock = new MockMcpClient();
  const batchOps =
    `[create(kind="entity", entity_kind="concept", name="LoRA"), create(kind="entity", entity_kind="concept", name="QLoRA")]`;
  mock.enqueue({
    results: [
      { ok: true, result: { id: "1", name: "LoRA" } },
      { ok: true, result: { id: "2", name: "QLoRA" } },
    ],
    summary: { total: 2, succeeded: 2, failed: 0 },
  });
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/request", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ ops: batchOps }),
    }),
  );

  assertEquals(res.status, 200);
  const body = await res.json();
  assertEquals(body.ok, true);
  assertEquals(mock.calls[0].ops, batchOps);
});

Deno.test("POST /api/request — 400 when ops missing", async () => {
  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/request", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ something: "else" }),
    }),
  );

  assertEquals(res.status, 400);
  assertEquals(mock.calls.length, 0);
});

Deno.test("POST /api/request — 400 on invalid JSON", async () => {
  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/request", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: "not-json",
    }),
  );

  assertEquals(res.status, 400);
});

Deno.test("POST /api/request — 200 even when individual ops fail (batch semantics)", async () => {
  const mock = new MockMcpClient();
  // Batch result with a failed op
  mock.enqueue({
    results: [
      { ok: true, result: { id: "1" } },
      { ok: false, error: "entity not found" },
    ],
    summary: { total: 2, succeeded: 1, failed: 1 },
  });
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/request", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ ops: '[get(id="a"), get(id="b")]' }),
    }),
  );

  // Batch: always 200, individual failures are in the result
  assertEquals(res.status, 200);
  const body = await res.json();
  assertEquals(body.ok, true);
  assertEquals(body.result.summary.failed, 1);
});
