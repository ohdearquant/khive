// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Unit tests for entity routes.
 *
 * Uses a mock McpClient — no live khive-mcp process required.
 * Tests cover happy paths, validation errors, and MCP error propagation.
 */

import { assertEquals } from "@std/assert";
import { createApp } from "../server.ts";
import type { McpClient, McpResult } from "../mcp/client.ts";

// ---------------------------------------------------------------------------
// Mock MCP client
// ---------------------------------------------------------------------------

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
// GET /api/entities
// ---------------------------------------------------------------------------

Deno.test("GET /api/entities — returns items", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ items: [{ id: "abc", kind: "concept", name: "LoRA" }] } as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/entities"));
  assertEquals(res.status, 200);
  const body = await res.json();
  assertEquals(body.ok, true);
  assertEquals(mock.calls.length, 1);
  assertEquals(mock.calls[0].ops, `list(kind="entity", limit=50, offset=0)`);
});

Deno.test("GET /api/entities?entity_kind=concept — passes entity_kind", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ items: [] } as McpResult);
  const app = makeApp(mock);

  await app.fetch(new Request("http://localhost/api/entities?entity_kind=concept&limit=10"));
  assertEquals(
    mock.calls[0].ops,
    `list(kind="entity", entity_kind="concept", limit=10, offset=0)`,
  );
});

// ---------------------------------------------------------------------------
// GET /api/entities/:id
// ---------------------------------------------------------------------------

Deno.test("GET /api/entities/:id — forwards id", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ kind: "entity", data: { id: "abc", name: "LoRA" } } as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/entities/abc12345"));
  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, `get(id="abc12345")`);
});

// ---------------------------------------------------------------------------
// POST /api/entities
// ---------------------------------------------------------------------------

Deno.test("POST /api/entities — creates entity", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ id: "new-id", kind: "concept", name: "FlashAttention" } as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/entities", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ kind: "concept", name: "FlashAttention" }),
    }),
  );

  assertEquals(res.status, 201);
  const body = await res.json();
  assertEquals(body.ok, true);
  assertEquals(
    mock.calls[0].ops,
    `create(kind="entity", entity_kind="concept", name="FlashAttention")`,
  );
});

Deno.test("POST /api/entities — 400 when kind missing", async () => {
  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/entities", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name: "NoKind" }),
    }),
  );

  assertEquals(res.status, 400);
  const body = await res.json();
  assertEquals(body.ok, false);
  assertEquals(body.error.code, "BAD_REQUEST");
  assertEquals(mock.calls.length, 0);
});

Deno.test("POST /api/entities — 400 on invalid JSON", async () => {
  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/entities", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: "not-json",
    }),
  );

  assertEquals(res.status, 400);
  const body = await res.json();
  assertEquals(body.error.code, "BAD_REQUEST");
});

// ---------------------------------------------------------------------------
// PATCH /api/entities/:id
// ---------------------------------------------------------------------------

Deno.test("PATCH /api/entities/:id — updates entity", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ id: "abc", name: "NewName" } as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/entities/abc", {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name: "NewName" }),
    }),
  );

  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, `update(id="abc", name="NewName")`);
});

// ---------------------------------------------------------------------------
// DELETE /api/entities/:id
// ---------------------------------------------------------------------------

Deno.test("DELETE /api/entities/:id — soft delete by default", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ deleted: true } as McpResult);
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/entities/abc", { method: "DELETE" }),
  );

  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, `delete(id="abc")`);
});

Deno.test("DELETE /api/entities/:id?hard=true — hard delete", async () => {
  const mock = new MockMcpClient();
  mock.enqueue({ deleted: true } as McpResult);
  const app = makeApp(mock);

  await app.fetch(
    new Request("http://localhost/api/entities/abc?hard=true", { method: "DELETE" }),
  );

  assertEquals(mock.calls[0].ops, `delete(id="abc", hard=true)`);
});

// ---------------------------------------------------------------------------
// Error propagation
// ---------------------------------------------------------------------------

Deno.test("GET /api/entities/:id — 404 on not-found error", async () => {
  const mock = new MockMcpClient();
  const app = makeApp(mock);

  // Override request to throw VerseError
  const { VerseError } = await import("../mcp/client.ts");
  mock.request = (_ops: string): Promise<McpResult> => {
    throw new VerseError("entity abc not found");
  };

  const res = await app.fetch(new Request("http://localhost/api/entities/abc"));
  assertEquals(res.status, 404);
  const body = await res.json();
  assertEquals(body.error.code, "NOT_FOUND");
});

Deno.test("GET /api/entities/:id — 500 on transport error", async () => {
  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const { McpTransportError } = await import("../mcp/client.ts");
  mock.request = (_ops: string): Promise<McpResult> => {
    throw new McpTransportError("stdio pipe broken");
  };

  const res = await app.fetch(new Request("http://localhost/api/entities/abc"));
  assertEquals(res.status, 500);
  const body = await res.json();
  assertEquals(body.error.code, "INTERNAL_SERVER_ERROR");
});
