import { describe, expect, it, vi } from "vitest";

import {
  createShowcaseAnalysisCatalogGet,
} from "@/lib/server/showcase-analysis-route";
import {
  ShowcaseAnalysisError,
  type ShowcaseAnalysisRegistry,
} from "@/lib/server/materialized-showcase-source";

describe("GET /api/showcase/analyses", () => {
  it("returns a strict deterministic v1 catalog without private fields", async () => {
    const registry: ShowcaseAnalysisRegistry = {
      root: "/server/private/analyses",
      entries: [
        {
          analysis_id: "zeta",
          canonical_url: "https://github.com/example/zeta",
        },
        {
          analysis_id: "alpha",
          canonical_url: "https://github.com/example/alpha",
        },
      ],
    };
    const loadRegistry = vi.fn(() => registry);
    const get = createShowcaseAnalysisCatalogGet(loadRegistry);

    const response = await get();

    expect(loadRegistry).toHaveBeenCalledOnce();
    expect(response.status).toBe(200);
    expect(response.headers.get("cache-control")).toBe("private, no-store");
    expect(response.headers.get("x-content-type-options")).toBe("nosniff");
    expect(response.headers.get("content-type")).toBe(
      "application/json; charset=utf-8",
    );
    const body = await response.text();
    expect(body).toBe(
      '{"schema_version":"khive.showcase.catalog.v1","entries":' +
        '[{"analysis_id":"alpha","canonical_url":' +
        '"https://github.com/example/alpha"},{"analysis_id":"zeta",' +
        '"canonical_url":"https://github.com/example/zeta"}]}',
    );
    expect(body).not.toContain(registry.root);
  });

  it("returns a sanitized private 404 when the catalog is unconfigured", async () => {
    const get = createShowcaseAnalysisCatalogGet(() => {
      throw new ShowcaseAnalysisError("NOT_CONFIGURED");
    });

    const response = await get();

    expect(response.status).toBe(404);
    expect(response.headers.get("cache-control")).toBe("private, no-store");
    expect(response.headers.get("x-content-type-options")).toBe("nosniff");
    await expect(response.json()).resolves.toEqual({
      error: {
        code: "NOT_CONFIGURED",
        message: "This repository analysis is not configured.",
      },
    });
  });
});
