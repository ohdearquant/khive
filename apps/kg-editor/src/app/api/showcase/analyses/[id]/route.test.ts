import { mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import { createShowcaseAnalysisGet } from "@/lib/server/showcase-analysis-route";
import {
  ShowcaseAnalysisError,
} from "@/lib/server/materialized-showcase-source";

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);
const temporaryRoots: string[] = [];
const authorized = () => true;
const unauthorized = () => false;

async function configuredAnalysis() {
  const root = await mkdtemp(resolve(tmpdir(), "khive-showcase-route-"));
  temporaryRoots.push(root);
  const report = resolve(root, "khive", "khive.repo.v1.json");
  await mkdir(dirname(report), { recursive: true });
  await writeFile(report, await readFile(goldenPath));
  return {
    root,
    entries: [{
      analysis_id: "khive",
      canonical_url: "https://github.com/ohdearquant/khive",
    }],
  };
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
        authorized,
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
      entries: [{
        analysis_id: "khive",
        canonical_url: "https://github.com/ohdearquant/khive",
      }],
    };
    const loadAnalysis = vi.fn();
    const get = createShowcaseAnalysisGet(
      () => registry,
      loadAnalysis,
      authorized,
    );

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

  it("sanitizes a configured report that fails identity validation", async () => {
    const registry = {
      root: "/a/server/private/path/that/must/not-be-returned",
      entries: [{
        analysis_id: "khive",
        canonical_url: "https://github.com/operator/private-repository",
      }],
    };
    const get = createShowcaseAnalysisGet(
      () => registry,
      async () => {
        throw new ShowcaseAnalysisError("ANALYSIS_INVALID");
      },
      authorized,
    );

    const response = await get(
      new Request("http://localhost/api/showcase/analyses/khive"),
      { params: Promise.resolve({ id: "khive" }) },
    );

    expect(response.status).toBe(500);
    expect(response.headers.get("cache-control")).toBe("private, no-store");
    expect(response.headers.get("x-content-type-options")).toBe("nosniff");
    const body = await response.text();
    expect(JSON.parse(body)).toEqual({
      error: {
        code: "ANALYSIS_INVALID",
        message: "This repository analysis did not pass validation.",
      },
    });
    expect(body).not.toContain(registry.root);
    expect(body).not.toContain(registry.entries[0].canonical_url);
  });

  it("rejects a configured analysis when the caller is not authorized", async () => {
    const registry = await configuredAnalysis();
    const loadAnalysis = vi.fn();
    const get = createShowcaseAnalysisGet(
      () => registry,
      loadAnalysis,
      unauthorized,
    );

    const response = await get(
      new Request("http://localhost/api/showcase/analyses/khive"),
      { params: Promise.resolve({ id: "khive" }) },
    );

    expect(response.status).toBe(404);
    const body = await response.text();
    expect(JSON.parse(body)).toEqual({
      error: {
        code: "NOT_CONFIGURED",
        message: "This repository analysis is not configured.",
      },
    });
    expect(loadAnalysis).not.toHaveBeenCalled();
  });

  it(
    "rejects the default request authorizer without a matching bearer token",
    async () => {
      vi.stubEnv("KHIVE_SHOWCASE_ACCESS_TOKEN", "operator-secret");
      const registry = await configuredAnalysis();
      const loadAnalysis = vi.fn();
      const get = createShowcaseAnalysisGet(() => registry, loadAnalysis);

      const noHeader = await get(
        new Request("http://localhost/api/showcase/analyses/khive"),
        { params: Promise.resolve({ id: "khive" }) },
      );
      const wrongToken = await get(
        new Request("http://localhost/api/showcase/analyses/khive", {
          headers: { authorization: "Bearer wrong-token" },
        }),
        { params: Promise.resolve({ id: "khive" }) },
      );

      expect(noHeader.status).toBe(404);
      expect(wrongToken.status).toBe(404);
      expect(loadAnalysis).not.toHaveBeenCalled();
      vi.unstubAllEnvs();
    },
  );
});
