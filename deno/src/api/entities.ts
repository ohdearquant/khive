// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Entity CRUD routes — ADR-044 D2.
 *
 * GET    /api/entities           → list(kind="entity", ...)
 * GET    /api/entities/:id       → get(id=":id")
 * POST   /api/entities           → create(kind="entity", ...)
 * PATCH  /api/entities/:id       → update(id=":id", ...)
 * DELETE /api/entities/:id       → delete(id=":id")
 */

import { Hono } from "@hono/hono";
import type { McpClient } from "../mcp/client.ts";
import {
  buildCreateEntity,
  buildDeleteEntity,
  buildGetEntity,
  buildListEntities,
  buildUpdateEntity,
} from "./dsl.ts";
import { getAuth } from "../auth/keys.ts";
import { handleMcpError, parseIntParam } from "./helpers.ts";
import type { CreateEntityBody, UpdateEntityBody } from "../types/api.ts";

export function createEntityRoutes(client: McpClient): Hono {
  const app = new Hono();

  // GET /api/entities
  app.get("/", async (c) => {
    const q = c.req.query();
    const auth = getAuth(c);
    const ops = buildListEntities({
      entity_kind: q.entity_kind ?? q.kind,
      limit: parseIntParam(q.limit, 50),
      offset: parseIntParam(q.offset, 0),
      namespace: auth.namespace,
    });
    try {
      const raw = await client.request(ops);
      // list returns the array directly or wrapped — normalise
      const items = Array.isArray(raw) ? raw : (raw.result ?? raw);
      return c.json({
        ok: true,
        result: {
          items,
          limit: parseIntParam(q.limit, 50),
          offset: parseIntParam(q.offset, 0),
        },
      });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  // GET /api/entities/:id
  app.get("/:id", async (c) => {
    const id = c.req.param("id");
    const auth = getAuth(c);
    const ops = buildGetEntity({ id, namespace: auth.namespace });
    try {
      const raw = await client.request(ops);
      // get returns { kind, data } wrapper — pass through
      return c.json({ ok: true, result: raw.result ?? raw });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  // POST /api/entities
  app.post("/", async (c) => {
    let body: CreateEntityBody;
    try {
      body = await c.req.json<CreateEntityBody>();
    } catch {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "invalid JSON body" } },
        400,
      );
    }

    if (!body.kind || !body.name) {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "kind and name are required" } },
        400,
      );
    }

    const auth = getAuth(c);
    const ops = buildCreateEntity({
      entity_kind: body.kind,
      name: body.name,
      description: body.description,
      namespace: body.namespace ?? auth.namespace,
      tags: body.tags,
      properties: body.properties,
    });

    try {
      const raw = await client.request(ops);
      return c.json({ ok: true, result: raw.result ?? raw }, 201);
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  // PATCH /api/entities/:id
  app.patch("/:id", async (c) => {
    const id = c.req.param("id");
    let body: UpdateEntityBody;
    try {
      body = await c.req.json<UpdateEntityBody>();
    } catch {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "invalid JSON body" } },
        400,
      );
    }

    const auth = getAuth(c);
    const ops = buildUpdateEntity({
      id,
      name: body.name,
      description: body.description,
      namespace: auth.namespace,
      tags: body.tags,
      properties: body.properties,
    });

    try {
      const raw = await client.request(ops);
      return c.json({ ok: true, result: raw.result ?? raw });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  // DELETE /api/entities/:id
  app.delete("/:id", async (c) => {
    const id = c.req.param("id");
    const hard = c.req.query("hard") === "true" ? true : undefined;
    const auth = getAuth(c);
    const ops = buildDeleteEntity({ id, hard, namespace: auth.namespace });
    try {
      const raw = await client.request(ops);
      return c.json({ ok: true, result: raw.result ?? raw });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  return app;
}
