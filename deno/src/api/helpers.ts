// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Shared helpers for route handlers: error mapping, param parsing.
 */

import type { Context } from "@hono/hono";
import { McpTransportError, VerseError } from "../mcp/client.ts";
import type { ErrorCode } from "../types/api.ts";
import { ERROR_STATUS } from "../types/api.ts";

// ---------------------------------------------------------------------------
// Error code inference from verb error messages
// ---------------------------------------------------------------------------

const NOT_FOUND_PATTERNS = [
  /not found/i,
  /no record/i,
  /does not exist/i,
  /unknown.*id/i,
];

const CONFLICT_PATTERNS = [
  /already exists/i,
  /duplicate/i,
  /conflict/i,
  /unique.*constraint/i,
];

const FORBIDDEN_PATTERNS = [
  /forbidden/i,
  /gate.*deny/i,
  /access.*denied/i,
  /not.*allowed/i,
];

const UNPROCESSABLE_PATTERNS = [
  /invalid.*kind/i,
  /unknown.*kind/i,
  /bad.*relation/i,
  /invalid.*relation/i,
  /validation.*fail/i,
  /invalid.*status/i,
  /cannot.*transition/i,
];

function inferErrorCode(message: string): ErrorCode {
  if (NOT_FOUND_PATTERNS.some((p) => p.test(message))) return "NOT_FOUND";
  if (CONFLICT_PATTERNS.some((p) => p.test(message))) return "CONFLICT";
  if (FORBIDDEN_PATTERNS.some((p) => p.test(message))) return "FORBIDDEN";
  if (UNPROCESSABLE_PATTERNS.some((p) => p.test(message))) return "UNPROCESSABLE_ENTITY";
  return "BAD_REQUEST";
}

// ---------------------------------------------------------------------------
// Central error handler
// ---------------------------------------------------------------------------

/**
 * Map a caught error to the consistent `{ok: false, error: {code, message}}`
 * HTTP response (ADR-044 D5).
 */
export function handleMcpError(c: Context, err: unknown): Response {
  if (err instanceof McpTransportError) {
    return c.json(
      {
        ok: false,
        error: { code: "INTERNAL_SERVER_ERROR", message: err.message },
      },
      500,
    );
  }

  if (err instanceof VerseError) {
    const code = inferErrorCode(err.message);
    const status = ERROR_STATUS[code] as 400 | 403 | 404 | 409 | 422;
    return c.json({ ok: false, error: { code, message: err.message } }, status);
  }

  const message = err instanceof Error ? err.message : String(err);
  return c.json(
    {
      ok: false,
      error: { code: "INTERNAL_SERVER_ERROR", message },
    },
    500,
  );
}

// ---------------------------------------------------------------------------
// Query param helpers
// ---------------------------------------------------------------------------

/** Parse an optional integer query param with a fallback. */
export function parseIntParam(v: string | undefined, fallback: number): number {
  if (v === undefined) return fallback;
  const n = parseInt(v, 10);
  return isNaN(n) ? fallback : n;
}

/** Parse a comma-separated query param into a string array. */
export function parseListParam(v: string | undefined): string[] | undefined {
  if (!v) return undefined;
  return v.split(",").map((s) => s.trim()).filter(Boolean);
}
