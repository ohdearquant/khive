// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Bearer token → namespace resolution (ADR-044 D3).
 *
 * Two tiers:
 *  - Local dev (KHIVE_AUTH_DISABLED=true): no auth required; returns
 *    default namespace from KHIVE_DEFAULT_NAMESPACE env var.
 *  - API key auth: bearer token resolved from a JSON key store at
 *    KHIVE_API_KEYS_FILE (default: ~/.khive/api-keys.json).
 *
 * Key store format:
 *   { "khive-sk-abc123": "lambda:khive", ... }
 *
 * Future: replaced by OAuth / DB lookup per ADR-034.
 */

import type { Context, MiddlewareHandler } from "@hono/hono";
import type { ResolvedAuth } from "../types/api.ts";

// ---------------------------------------------------------------------------
// Key store
// ---------------------------------------------------------------------------

type KeyStore = Record<string, string>; // token → namespace

let _keyStore: KeyStore | null = null;

async function loadKeyStore(): Promise<KeyStore> {
  if (_keyStore !== null) return _keyStore;

  const path = Deno.env.get("KHIVE_API_KEYS_FILE") ??
    `${Deno.env.get("HOME") ?? "~"}/.khive/api-keys.json`;

  try {
    const text = await Deno.readTextFile(path);
    _keyStore = JSON.parse(text) as KeyStore;
  } catch {
    // File missing or unreadable → empty store (all tokens rejected)
    _keyStore = {};
  }

  return _keyStore;
}

/** Resolve a bearer token to a namespace. Returns undefined if not found. */
export async function resolveToken(token: string): Promise<string | undefined> {
  const store = await loadKeyStore();
  return store[token];
}

/** Invalidate the in-memory key store cache (for tests). */
export function resetKeyStoreCache(): void {
  _keyStore = null;
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/** Hono context key for the resolved auth info. */
export const AUTH_KEY = "auth";

/**
 * Auth middleware (ADR-044 D3).
 *
 * When KHIVE_AUTH_DISABLED=true (default in dev), skips token validation and
 * sets namespace from KHIVE_DEFAULT_NAMESPACE (or undefined for server default).
 *
 * When auth is enabled, extracts the `Authorization: Bearer <token>` header,
 * resolves it to a namespace, and stores in context. Returns 401 if missing
 * or invalid.
 */
export function authMiddleware(): MiddlewareHandler {
  return async (c: Context, next) => {
    const authDisabled = (Deno.env.get("KHIVE_AUTH_DISABLED") ?? "true").toLowerCase() === "true";

    if (authDisabled) {
      const ns = Deno.env.get("KHIVE_DEFAULT_NAMESPACE");
      c.set(AUTH_KEY, { namespace: ns } satisfies ResolvedAuth);
      await next();
      return;
    }

    // Extract bearer token
    const authHeader = c.req.header("Authorization");
    if (!authHeader?.startsWith("Bearer ")) {
      return c.json(
        {
          ok: false,
          error: { code: "UNAUTHORIZED", message: "missing or invalid Authorization header" },
        },
        401,
      );
    }

    const token = authHeader.slice("Bearer ".length).trim();
    const namespace = await resolveToken(token);

    if (namespace === undefined) {
      return c.json(
        { ok: false, error: { code: "UNAUTHORIZED", message: "invalid API key" } },
        401,
      );
    }

    c.set(AUTH_KEY, { namespace } satisfies ResolvedAuth);
    await next();
  };
}

/** Extract resolved auth from Hono context. */
export function getAuth(c: Context): ResolvedAuth {
  return (c.get(AUTH_KEY) as ResolvedAuth | undefined) ?? {};
}
