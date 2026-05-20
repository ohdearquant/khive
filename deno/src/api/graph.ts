// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Graph traversal routes — ADR-044 D2.
 *
 * GET /api/traverse               → traverse(roots=[":id"], ...)
 * GET /api/entities/:id/neighbors → neighbors(node_id=":id", ...)
 */

import { Hono } from "@hono/hono";
import type { McpClient } from "../mcp/client.ts";
import { buildNeighbors, buildTraverse } from "./dsl.ts";
import { getAuth } from "../auth/keys.ts";
import { handleMcpError, parseIntParam, parseListParam } from "./helpers.ts";

/**
 * Traverse route — mounted at /api/traverse in server.ts.
 */
export function createTraverseRoute(client: McpClient): Hono {
  const app = new Hono();

  // GET /api/traverse?roots=id1,id2&max_depth=N&relations=rel1,rel2
  app.get("/", async (c) => {
    const q = c.req.query();
    const rootsRaw = q.roots;

    if (!rootsRaw) {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "roots parameter is required" } },
        400,
      );
    }

    const roots = rootsRaw.split(",").map((r) => r.trim()).filter(Boolean);
    const auth = getAuth(c);
    const ops = buildTraverse({
      roots,
      max_depth: parseIntParam(q.max_depth, 3),
      relations: parseListParam(q.relations),
      direction: q.direction,
      namespace: auth.namespace,
    });

    try {
      const raw = await client.request(ops);
      const nodes = Array.isArray(raw) ? raw : (raw.result ?? raw);
      return c.json({
        ok: true,
        result: { nodes, total: Array.isArray(nodes) ? nodes.length : 0 },
      });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  return app;
}

/**
 * Neighbors route — mounted at /api/entities in server.ts as a sub-route
 * alongside the CRUD routes.
 *
 * GET /:id/neighbors?direction=out&relations=implements,extends
 */
export function createNeighborsRoute(client: McpClient): Hono {
  const app = new Hono();

  app.get("/:id/neighbors", async (c) => {
    const id = c.req.param("id");
    const q = c.req.query();
    const auth = getAuth(c);
    const ops = buildNeighbors({
      node_id: id,
      direction: q.direction,
      relations: parseListParam(q.relations),
      namespace: auth.namespace,
    });

    try {
      const raw = await client.request(ops);
      const nodes = Array.isArray(raw) ? raw : (raw.result ?? raw);
      return c.json({
        ok: true,
        result: { nodes, total: Array.isArray(nodes) ? nodes.length : 0 },
      });
    } catch (err) {
      return handleMcpError(c, err);
    }
  });

  return app;
}
