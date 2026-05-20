// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * GTD task routes — ADR-044 D2.
 *
 * Only available when KHIVE_PACKS contains "gtd".
 *
 * GET  /api/tasks                   → tasks(status=?, assignee=?, priority=?)
 * POST /api/tasks                   → assign(title=..., ...)
 * POST /api/tasks/:id/transition    → transition(id=":id", status=...)
 * POST /api/tasks/:id/complete      → complete(id=":id", ...)
 */

import { Hono } from "@hono/hono";
import type { McpClient } from "../mcp/client.ts";
import { buildCompleteTask, buildCreateTask, buildListTasks, buildTransitionTask } from "./dsl.ts";
import { getAuth } from "../auth/keys.ts";
import { handleMcpError, parseIntParam } from "./helpers.ts";
import type { CompleteTaskBody, CreateTaskBody, TransitionTaskBody } from "../types/api.ts";

/** Returns true when the GTD pack is loaded. */
function isGtdAvailable(): boolean {
  const packs = Deno.env.get("KHIVE_PACKS") ?? "";
  return packs.split(",").map((p) => p.trim()).includes("gtd");
}

export function createTaskRoutes(client: McpClient): Hono {
  const app = new Hono();

  // Guard middleware — all task routes require GTD pack
  app.use("*", async (c, next) => {
    if (!isGtdAvailable()) {
      return c.json(
        {
          ok: false,
          error: {
            code: "NOT_IMPLEMENTED",
            message: "GTD pack not loaded — set KHIVE_PACKS=kg,gtd",
          },
        },
        501,
      );
    }
    await next();
  });

  // GET /api/tasks
  app.get("/", async (c) => {
    const q = c.req.query();
    const auth = getAuth(c);
    const ops = buildListTasks({
      status: q.status,
      assignee: q.assignee,
      priority: q.priority,
      limit: parseIntParam(q.limit, 50),
      offset: parseIntParam(q.offset, 0),
      namespace: auth.namespace,
    });
    try {
      const raw = await client.request(ops);
      const items = Array.isArray(raw) ? raw : (raw.result ?? raw);
      return c.json({ ok: true, result: { items } });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  // POST /api/tasks
  app.post("/", async (c) => {
    let body: CreateTaskBody;
    try {
      body = await c.req.json<CreateTaskBody>();
    } catch {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "invalid JSON body" } },
        400,
      );
    }

    if (!body.title) {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "title is required" } },
        400,
      );
    }

    const auth = getAuth(c);
    const ops = buildCreateTask({
      title: body.title,
      priority: body.priority,
      status: body.status,
      assignee: body.assignee,
      due: body.due,
      tags: body.tags,
      depends_on: body.depends_on,
      namespace: auth.namespace,
    });

    try {
      const raw = await client.request(ops);
      return c.json({ ok: true, result: raw.result ?? raw }, 201);
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  // POST /api/tasks/:id/transition
  app.post("/:id/transition", async (c) => {
    const id = c.req.param("id");
    let body: TransitionTaskBody;
    try {
      body = await c.req.json<TransitionTaskBody>();
    } catch {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "invalid JSON body" } },
        400,
      );
    }

    if (!body.status) {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "status is required" } },
        400,
      );
    }

    const auth = getAuth(c);
    const ops = buildTransitionTask({
      id,
      status: body.status,
      note: body.note,
      namespace: auth.namespace,
    });

    try {
      const raw = await client.request(ops);
      return c.json({ ok: true, result: raw.result ?? raw });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  // POST /api/tasks/:id/complete
  app.post("/:id/complete", async (c) => {
    const id = c.req.param("id");
    let body: CompleteTaskBody = {};
    try {
      body = await c.req.json<CompleteTaskBody>();
    } catch {
      // body is optional for complete
    }

    const auth = getAuth(c);
    const ops = buildCompleteTask({
      id,
      result: body.result,
      namespace: auth.namespace,
    });

    try {
      const raw = await client.request(ops);
      return c.json({ ok: true, result: raw.result ?? raw });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  return app;
}
