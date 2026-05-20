// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * khive HTTP gateway — ADR-044.
 *
 * Hono application that wraps `khive-mcp` (spawned as a child process) and
 * exposes a REST API for the frontend dashboard and external integrations.
 *
 * Every route translates HTTP params → DSL ops string → `request` MCP tool
 * → JSON response. No business logic lives in this layer.
 *
 * Startup sequence (ADR-044 D8):
 *  1. Read config from env
 *  2. Spawn `khive-mcp` child process
 *  3. MCP handshake (5s timeout)
 *  4. Register routes + start HTTP server
 *  5. SIGTERM/SIGINT → graceful shutdown
 */

import { Hono } from "@hono/hono";
import { cors } from "@hono/hono/cors";
import { logger } from "@hono/hono/logger";

import { KhiveMcpClient, type McpClient } from "./mcp/client.ts";
import { authMiddleware } from "./auth/keys.ts";
import { createEntityRoutes } from "./api/entities.ts";
import { createEdgeRoutes } from "./api/edges.ts";
import { createTaskRoutes } from "./api/tasks.ts";
import { createSearchRoutes } from "./api/search.ts";
import { createNeighborsRoute, createTraverseRoute } from "./api/graph.ts";
import { createRequestRoutes } from "./api/request.ts";
import { createEventRoutes } from "./api/events.ts";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const PORT = Number(Deno.env.get("KHIVE_PORT") ?? Deno.env.get("PORT") ?? "8000");
const AUTH_DISABLED = (Deno.env.get("KHIVE_AUTH_DISABLED") ?? "true").toLowerCase() === "true";
const CORS_ORIGINS = (Deno.env.get("KHIVE_CORS_ORIGINS") ?? "http://localhost:3000")
  .split(",")
  .map((s) => s.trim());

// ---------------------------------------------------------------------------
// MCP client startup
// ---------------------------------------------------------------------------

let mcpClient: KhiveMcpClient;

async function startMcpClient(): Promise<KhiveMcpClient> {
  const HANDSHAKE_TIMEOUT_MS = 5000;

  const connectPromise = KhiveMcpClient.connect();
  const timeoutPromise = new Promise<never>((_, reject) =>
    setTimeout(
      () => reject(new Error("khive-mcp handshake timed out after 5 seconds")),
      HANDSHAKE_TIMEOUT_MS,
    )
  );

  try {
    const client = await Promise.race([connectPromise, timeoutPromise]);
    return client;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error(`[server] failed to connect to khive-mcp: ${msg}`);
    console.error("[server] ensure khive-mcp is installed and on PATH (or set KHIVE_MCP_BIN)");
    Deno.exit(1);
  }
}

// ---------------------------------------------------------------------------
// App factory (exported for tests)
// ---------------------------------------------------------------------------

export function createApp(client: McpClient): Hono {
  const app = new Hono();

  // Logging
  app.use("*", logger());

  // CORS (ADR-044 D4)
  app.use(
    "*",
    cors({
      origin: (origin) => (CORS_ORIGINS.includes(origin) ? origin : null),
      allowMethods: ["GET", "POST", "PATCH", "DELETE", "OPTIONS"],
      allowHeaders: ["Content-Type", "Authorization"],
      maxAge: 86400,
    }),
  );

  // Auth middleware (ADR-044 D3)
  app.use("/api/*", authMiddleware());

  // Health check (no auth)
  app.get("/health", (c) =>
    c.json({
      ok: true,
      result: {
        status: "ok",
        service: "khive-server",
        version: "0.1.0",
        auth_disabled: AUTH_DISABLED,
      },
    }));

  // ---------------------------------------------------------------------------
  // API routes
  // ---------------------------------------------------------------------------

  // Entity CRUD + neighbors (both mounted at /api/entities)
  app.route("/api/entities", createEntityRoutes(client));
  app.route("/api/entities", createNeighborsRoute(client));

  // Edge routes
  app.route("/api/edges", createEdgeRoutes(client));

  // Task routes (GTD pack)
  app.route("/api/tasks", createTaskRoutes(client));

  // Search
  app.route("/api/search", createSearchRoutes(client));

  // Graph traversal
  app.route("/api/traverse", createTraverseRoute(client));

  // Raw DSL passthrough
  app.route("/api/request", createRequestRoutes(client));

  // Event stream (phase 2 stub)
  app.route("/api/events", createEventRoutes());

  // 404 fallback
  app.notFound((c) =>
    c.json(
      {
        ok: false,
        error: { code: "NOT_FOUND", message: `no route: ${c.req.method} ${c.req.path}` },
      },
      404,
    )
  );

  return app;
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

if (import.meta.main) {
  if (AUTH_DISABLED) {
    console.warn(
      "[server] WARNING: auth is disabled (KHIVE_AUTH_DISABLED=true). Do not use in production.",
    );
  }

  console.log("[server] connecting to khive-mcp...");
  mcpClient = await startMcpClient();
  console.log("[server] khive-mcp connected");

  const app = createApp(mcpClient);

  // Graceful shutdown
  const shutdown = async () => {
    console.log("[server] shutting down...");
    try {
      await Promise.race([
        mcpClient.close(),
        new Promise<void>((resolve) => setTimeout(resolve, 2000)),
      ]);
    } finally {
      Deno.exit(0);
    }
  };

  Deno.addSignalListener("SIGTERM", shutdown);
  Deno.addSignalListener("SIGINT", shutdown);

  console.log(`[server] listening on http://localhost:${PORT}`);
  Deno.serve({ port: PORT }, app.fetch);
}
