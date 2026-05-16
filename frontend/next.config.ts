import type { NextConfig } from "next";

// Per ADR-003 the frontend has no business logic — it calls the Deno HTTP
// gateway (the `deno/` layer, which spawns khive-mcp). The rewrite target is
// the Deno gateway base URL; default matches `deno task server` (Hono :8000).
const KHIVE_GATEWAY_URL = process.env.NEXT_PUBLIC_KHIVE_GATEWAY_URL ??
  "http://localhost:8000";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  async rewrites() {
    return [
      {
        source: "/api/server/:path*",
        destination: `${KHIVE_GATEWAY_URL}/:path*`,
      },
    ];
  },
};

export default nextConfig;
