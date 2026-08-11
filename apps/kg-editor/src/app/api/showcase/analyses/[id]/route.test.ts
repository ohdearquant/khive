import { mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import { createShowcaseAnalysisGet } from "./route";

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);
const temporaryRoots: string[] = [];

async function configuredAnalysis() {
  const root = await mkdtemp(resolve(tmpdir(), "khive-showcase-route-"));
  temporaryRoots.push(root);
  const report = resolve(root, "khive", "khive.repo.v1.json");
  await mkdir(dirname(report), { recursive: true });
  await writeFile(report, await readFile(goldenPath));
  return { root, ids: new Set(["khive"]) };
}

afterEach(async () => {
  const { rm } = await import("node:fs/promises");
  await Promise.all(
    temporaryRoots.splice(0).map((root) => rm(root, { recursive: true })),
  );
});

describe("GET /api/showcase/analyses/[id]", () => {
  it(
    "serves a validated private snapshot with explicit provenance and no caching",
    async () => {
      const registry = await configuredAnalysis();
      const bytes = Uint8Array.from(await readFile(goldenPath));
      const get = createShowcaseAnalysisGet(
        () => registry,
        async () => ({ bytes, etag: `"sha256-${"a".repeat(64)}"` }),
      );

      const response = await get(
        new Request("http://localhost/api/showcase/analyses/khive"),
        {
          params: Promise.resolve({ id: "khive" }),
        },
      );

      expect(response.status).toBe(200);
      expect(response.headers.get("cache-control")).toBe("private, no-store");
      expect(response.headers.get("x-khive-analysis-source")).toBe(
        "khive-db-snapshot",
      );
      expect(response.headers.get("x-khive-analysis-id")).toBe("khive");
      expect(response.headers.get("etag")).toMatch(/^"sha256-[0-9a-f]{64}"$/);
      await expect(response.json()).resolves.toMatchObject({
        schema_version: "khive.repo.v1",
      });
    },
    20_000,
  );

  it("returns a stable sanitized error envelope for an unknown id", async () => {
    const registry = {
      root: "/a/server/private/path/that/must/not/be-read",
      ids: new Set(["khive"]),
    };
    const loadAnalysis = vi.fn();
    const get = createShowcaseAnalysisGet(() => registry, loadAnalysis);

    const response = await get(
      new Request("http://localhost/api/showcase/analyses/missing"),
      {
        params: Promise.resolve({ id: "missing" }),
      },
    );

    expect(response.status).toBe(404);
    expect(response.headers.get("cache-control")).toBe("private, no-store");
    const body = await response.text();
    expect(JSON.parse(body)).toEqual({
      error: {
        code: "NOT_CONFIGURED",
        message: "This repository analysis is not configured.",
      },
    });
    expect(body).not.toContain(registry.root);
    expect(loadAnalysis).not.toHaveBeenCalled();
  });
});
