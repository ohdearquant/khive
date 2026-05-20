// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Raw DSL passthrough — ADR-044 D2.
 *
 * POST /api/request
 *   Body: { ops: string }   → forwarded verbatim to the `request` MCP tool
 *
 * Per ADR-020 batch semantics: individual op failures do NOT become HTTP
 * errors. Always returns 200 with per-op `ok` discriminants in the results
 * array.
 */

import { Hono } from "@hono/hono";
import type { McpClient } from "../mcp/client.ts";
import { McpTransportError } from "../mcp/client.ts";
import type { RequestBody } from "../types/api.ts";

export function createRequestRoutes(client: McpClient): Hono {
  const app = new Hono();

  // POST /api/request
  app.post("/", async (c) => {
    let body: RequestBody;
    try {
      body = await c.req.json<RequestBody>();
    } catch {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "invalid JSON body" } },
        400,
      );
    }

    if (!body.ops || typeof body.ops !== "string") {
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message: "ops string is required" } },
        400,
      );
    }

    try {
      const raw = await client.request(body.ops);
      // Always 200 — individual op failures are inside the result (ADR-020 batch semantics)
      return c.json({ ok: true, result: raw });
    } catch (err) {
      if (err instanceof McpTransportError) {
        return c.json(
          { ok: false, error: { code: "INTERNAL_SERVER_ERROR", message: err.message } },
          500,
        );
      }
      // DSL parse error → 400
      const message = err instanceof Error ? err.message : String(err);
      return c.json(
        { ok: false, error: { code: "BAD_REQUEST", message } },
        400,
      );
    }
  });

  return app;
}
