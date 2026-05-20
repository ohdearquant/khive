// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Unit tests for the auth key store and middleware.
 */

import { assertEquals } from "@std/assert";
import { resetKeyStoreCache, resolveToken } from "./keys.ts";
import { createApp } from "../server.ts";
import type { McpClient, McpResult } from "../mcp/client.ts";

class MockMcpClient implements McpClient {
  request(_ops: string): Promise<McpResult> {
    return Promise.resolve({ items: [] } as McpResult);
  }

  close(): Promise<void> {
    return Promise.resolve();
  }
}

function makeApp(mock: McpClient) {
  return createApp(mock);
}

// ---------------------------------------------------------------------------
// Key store resolution
// ---------------------------------------------------------------------------

Deno.test("resolveToken: returns undefined for unknown token", async () => {
  resetKeyStoreCache();
  // No key file configured → empty store
  Deno.env.set("KHIVE_API_KEYS_FILE", "/nonexistent/path/api-keys.json");
  const ns = await resolveToken("unknown-token");
  assertEquals(ns, undefined);
  Deno.env.delete("KHIVE_API_KEYS_FILE");
  resetKeyStoreCache();
});

// ---------------------------------------------------------------------------
// Auth middleware: disabled (default dev mode)
// ---------------------------------------------------------------------------

Deno.test("auth disabled: /api/entities accessible without Authorization", async () => {
  Deno.env.set("KHIVE_AUTH_DISABLED", "true");

  const mock = new MockMcpClient();
  mock.request = (_ops: string) => Promise.resolve({ items: [] } as McpResult);

  const app = makeApp(mock);
  const res = await app.fetch(new Request("http://localhost/api/entities"));
  assertEquals(res.status, 200);

  Deno.env.delete("KHIVE_AUTH_DISABLED");
});

// ---------------------------------------------------------------------------
// Auth middleware: enabled
// ---------------------------------------------------------------------------

Deno.test("auth enabled: 401 when Authorization header missing", async () => {
  Deno.env.set("KHIVE_AUTH_DISABLED", "false");
  Deno.env.set("KHIVE_API_KEYS_FILE", "/nonexistent/path/api-keys.json");
  resetKeyStoreCache();

  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(new Request("http://localhost/api/entities"));
  assertEquals(res.status, 401);
  const body = await res.json();
  assertEquals(body.error.code, "UNAUTHORIZED");

  Deno.env.delete("KHIVE_AUTH_DISABLED");
  Deno.env.delete("KHIVE_API_KEYS_FILE");
  resetKeyStoreCache();
});

Deno.test("auth enabled: 401 on invalid token", async () => {
  Deno.env.set("KHIVE_AUTH_DISABLED", "false");
  Deno.env.set("KHIVE_API_KEYS_FILE", "/nonexistent/path/api-keys.json");
  resetKeyStoreCache();

  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(
    new Request("http://localhost/api/entities", {
      headers: { Authorization: "Bearer invalid-token" },
    }),
  );
  assertEquals(res.status, 401);

  Deno.env.delete("KHIVE_AUTH_DISABLED");
  Deno.env.delete("KHIVE_API_KEYS_FILE");
  resetKeyStoreCache();
});

// ---------------------------------------------------------------------------
// Health check: no auth required
// ---------------------------------------------------------------------------

Deno.test("GET /health — accessible without auth", async () => {
  Deno.env.set("KHIVE_AUTH_DISABLED", "false");

  const mock = new MockMcpClient();
  const app = makeApp(mock);

  const res = await app.fetch(new Request("http://localhost/health"));
  assertEquals(res.status, 200);
  const body = await res.json();
  assertEquals(body.ok, true);

  Deno.env.delete("KHIVE_AUTH_DISABLED");
});
