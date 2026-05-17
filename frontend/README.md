# khive — Frontend

Research knowledge-graph visualization and session UI. Per
[ADR-003](../docs/adr/ADR-003-four-layer-architecture.md), the frontend has no
business logic — it calls the **Deno HTTP gateway** (the `deno/` layer, which
spawns `khive-mcp`).

## Stack

- Next.js 15 (app router) · React 19 · TypeScript 5.7
- TailwindCSS 3.4 · `reactflow` (graph viz) · TanStack Query · zod
- ESLint 9 (flat config) · Prettier · Vitest

## Gateway contract

`next.config.ts` rewrites `/api/server/:path*` to the Deno gateway. Set the
base URL via `NEXT_PUBLIC_KHIVE_GATEWAY_URL` (defaults to `http://localhost:8000`,
matching `deno task server`).

## Status

**Scaffold only.** Not yet build-verified:

- `pnpm install` + `pnpm build` not run (no lockfile committed yet)
- First `pnpm install` on a follow-up pins deps and regenerates `next-env.d.ts`

## Develop

```bash
pnpm install
pnpm dev          # next dev (Turbopack)
pnpm build        # production build
pnpm lint         # eslint
pnpm test         # vitest
```
