// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Search route — ADR-044 D2.
 *
 * GET /api/search → search(kind=<q?>, query=<q>)
 */

import { Hono } from "@hono/hono";
import type { McpClient } from "../mcp/client.ts";
import { buildSearch } from "./dsl.ts";
import { getAuth } from "../auth/keys.ts";
import { handleMcpError, parseIntParam } from "./helpers.ts";

export function createSearchRoutes(client: McpClient): Hono {
  const app = new Hono();

  // GET /api/search?query=<q>&kind=<k>&limit=<n>
  app.get("/", async (c) => {
    const q = c.req.query();
    const query = q.query;

    if (!query) {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "query parameter is required" } },
        400,
      );
    }

    const auth = getAuth(c);
    const ops = buildSearch({
      query,
      kind: q.kind,
      limit: parseIntParam(q.limit, 20),
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

  return app;
}
