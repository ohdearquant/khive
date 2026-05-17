import { Hono } from "@hono/hono";
import { cors } from "@hono/hono/cors";
import { logger } from "@hono/hono/logger";
import { health } from "./api/health.ts";

const app = new Hono();

app.use("*", logger());
app.use("*", cors());

app.route("/health", health);

const port = Number(Deno.env.get("PORT") ?? "8000");

console.log(`khive-server listening on http://localhost:${port}`);

Deno.serve({ port }, app.fetch);
