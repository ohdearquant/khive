import { Hono } from "@hono/hono";

export const health = new Hono();

health.get("/", (c) =>
  c.json({
    status: "ok",
    service: "khive-server",
    version: "0.1.0",
  }));
