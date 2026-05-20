// Copyright 2024 khive contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

/**
 * Event stream stub — ADR-044 D6.
 *
 * WebSocket and SSE endpoints are deferred to phase 2, pending ADR-038
 * (events surface). This stub reserves the path and returns 501.
 */

import { Hono } from "@hono/hono";

export function createEventRoutes(): Hono {
  const app = new Hono();

  // WS /api/events — reserved, returns 501
  app.get("/", (c) => {
    console.log("[events] event stream requested — pending ADR-038");
    return c.json(
      {
        ok: false,
        error: {
          code: "NOT_IMPLEMENTED",
          message: "event stream requires ADR-038",
        },
      },
      501,
    );
  });

  return app;
}
