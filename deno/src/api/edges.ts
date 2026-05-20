// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Edge routes — ADR-044 D2.
 *
 * GET    /api/edges       → list(kind="edge", ...)
 * POST   /api/edges       → link(source_id=..., target_id=..., relation=...)
 * DELETE /api/edges/:id   → delete(id=":id")
 */

import { Hono } from "@hono/hono";
import type { McpClient } from "../mcp/client.ts";
import { buildCreateEdge, buildDeleteEdge, buildListEdges } from "./dsl.ts";
import { getAuth } from "../auth/keys.ts";
import { handleMcpError } from "./helpers.ts";
import type { CreateEdgeBody } from "../types/api.ts";

export function createEdgeRoutes(client: McpClient): Hono {
  const app = new Hono();

  // GET /api/edges
  app.get("/", async (c) => {
    const q = c.req.query();
    const auth = getAuth(c);
    const ops = buildListEdges({
      source_id: q.source_id,
      target_id: q.target_id,
      relation: q.relation,
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

  // POST /api/edges
  app.post("/", async (c) => {
    let body: CreateEdgeBody;
    try {
      body = await c.req.json<CreateEdgeBody>();
    } catch {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "invalid JSON body" } },
        400,
      );
    }

    if (!body.source_id || !body.target_id || !body.relation) {
      return c.json(
        {
          ok: false,
          error: {
            code: "BAD_REQUEST",
            message: "source_id, target_id, and relation are required",
          },
        },
        400,
      );
    }

    const auth = getAuth(c);
    const ops = buildCreateEdge({
      source_id: body.source_id,
      target_id: body.target_id,
      relation: body.relation,
      weight: body.weight,
      namespace: body.namespace ?? auth.namespace,
    });

    try {
      const raw = await client.request(ops);
      return c.json({ ok: true, result: raw.result ?? raw }, 201);
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  // DELETE /api/edges/:id
  app.delete("/:id", async (c) => {
    const id = c.req.param("id");
    const auth = getAuth(c);
    const ops = buildDeleteEdge({ id, namespace: auth.namespace });
    try {
      const raw = await client.request(ops);
      return c.json({ ok: true, result: raw.result ?? raw });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  return app;
}
