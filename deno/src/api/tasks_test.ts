// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Unit tests for task routes.
 *
 * GTD pack availability is controlled via KHIVE_PACKS env var.
 * Tests that need GTD routes must set it before calling the app.
 */

import { assertEquals } from "@std/assert";
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

// ---------------------------------------------------------------------------
// GTD unavailable (default — KHIVE_PACKS not set)
// ---------------------------------------------------------------------------

Deno.test("GET /api/tasks — 501 when GTD pack not loaded", async () => {
  // Ensure GTD is not in packs
  Deno.env.delete("KHIVE_PACKS");

  const { createApp } = await import("../server.ts");
  const mock = new MockMcpClient();
  const app = createApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/tasks"));
  assertEquals(res.status, 501);
  const body = await res.json();
  assertEquals(body.ok, false);
  assertEquals(body.error.code, "NOT_IMPLEMENTED");
  assertEquals(mock.calls.length, 0);
});

// ---------------------------------------------------------------------------
// GTD available
// ---------------------------------------------------------------------------

Deno.test("GET /api/tasks — lists tasks when GTD available", async () => {
  Deno.env.set("KHIVE_PACKS", "kg,gtd");

  const { createApp } = await import("../server.ts");
  const mock = new MockMcpClient();
  mock.enqueue({ items: [] } as McpResult);
  const app = createApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/tasks"));
  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, "tasks(limit=50, offset=0)");

  Deno.env.delete("KHIVE_PACKS");
});

Deno.test("GET /api/tasks?status=next — filters by status", async () => {
  Deno.env.set("KHIVE_PACKS", "kg,gtd");

  const { createApp } = await import("../server.ts");
  const mock = new MockMcpClient();
  mock.enqueue({ items: [] } as McpResult);
  const app = createApp(mock);

  await app.fetch(new Request("http://localhost/api/tasks?status=next&priority=p0"));
  assertEquals(mock.calls[0].ops, `tasks(status="next", priority="p0", limit=50, offset=0)`);

  Deno.env.delete("KHIVE_PACKS");
});

Deno.test("POST /api/tasks — creates task", async () => {
  Deno.env.set("KHIVE_PACKS", "kg,gtd");

  const { createApp } = await import("../server.ts");
  const mock = new MockMcpClient();
  mock.enqueue({ id: "task-1", title: "Fix bug" } as McpResult);
  const app = createApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/tasks", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ title: "Fix bug", priority: "p0" }),
    }),
  );

  assertEquals(res.status, 201);
  assertEquals(mock.calls[0].ops, `assign(title="Fix bug", priority="p0")`);

  Deno.env.delete("KHIVE_PACKS");
});

Deno.test("POST /api/tasks — 400 when title missing", async () => {
  Deno.env.set("KHIVE_PACKS", "kg,gtd");

  const { createApp } = await import("../server.ts");
  const mock = new MockMcpClient();
  const app = createApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/tasks", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ priority: "p0" }),
    }),
  );

  assertEquals(res.status, 400);
  assertEquals(mock.calls.length, 0);

  Deno.env.delete("KHIVE_PACKS");
});

Deno.test("POST /api/tasks/:id/transition — transitions task", async () => {
  Deno.env.set("KHIVE_PACKS", "kg,gtd");

  const { createApp } = await import("../server.ts");
  const mock = new MockMcpClient();
  mock.enqueue({ id: "task-1", status: "active" } as McpResult);
  const app = createApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/tasks/task-1/transition", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ status: "active", note: "started" }),
    }),
  );

  assertEquals(res.status, 200);
  assertEquals(
    mock.calls[0].ops,
    `transition(id="task-1", status="active", note="started")`,
  );

  Deno.env.delete("KHIVE_PACKS");
});

Deno.test("POST /api/tasks/:id/complete — completes task", async () => {
  Deno.env.set("KHIVE_PACKS", "kg,gtd");

  const { createApp } = await import("../server.ts");
  const mock = new MockMcpClient();
  mock.enqueue({ id: "task-1", status: "done" } as McpResult);
  const app = createApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/tasks/task-1/complete", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ result: "PR merged" }),
    }),
  );

  assertEquals(res.status, 200);
  assertEquals(mock.calls[0].ops, `complete(id="task-1", result="PR merged")`);

  Deno.env.delete("KHIVE_PACKS");
});
