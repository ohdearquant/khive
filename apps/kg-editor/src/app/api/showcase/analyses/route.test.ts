import { describe, expect, it, vi } from "vitest";

import {
  createShowcaseAnalysisCatalogGet,
} from "@/lib/server/showcase-analysis-route";
import {
  ShowcaseAnalysisError,
  type ShowcaseAnalysisRegistry,
} from "@/lib/server/materialized-showcase-source";

const authorized = () => true;
const unauthorized = () => false;
const anonymousRequest = () =>
  new Request("http://localhost/api/showcase/analyses");

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
    const get = createShowcaseAnalysisCatalogGet(loadRegistry, authorized);

    const response = await get(anonymousRequest());

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
    }, authorized);

    const response = await get(anonymousRequest());

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

  it("rejects the catalog when the caller is not authorized", async () => {
    const loadRegistry = vi.fn();
    const get = createShowcaseAnalysisCatalogGet(loadRegistry, unauthorized);

    const response = await get(anonymousRequest());

    expect(response.status).toBe(404);
    const body = await response.text();
    expect(JSON.parse(body)).toEqual({
      error: {
        code: "NOT_CONFIGURED",
        message: "This repository analysis is not configured.",
      },
    });
    expect(loadRegistry).not.toHaveBeenCalled();
  });

  it(
    "rejects the default request authorizer without a matching bearer token",
    async () => {
      vi.stubEnv("KHIVE_SHOWCASE_ACCESS_TOKEN", "operator-secret");
      const loadRegistry = vi.fn();
      const get = createShowcaseAnalysisCatalogGet(loadRegistry);

      const noHeader = await get(anonymousRequest());
      const wrongToken = await get(
        new Request("http://localhost/api/showcase/analyses", {
          headers: { authorization: "Bearer wrong-token" },
        }),
      );

      expect(noHeader.status).toBe(404);
      expect(wrongToken.status).toBe(404);
      expect(loadRegistry).not.toHaveBeenCalled();
      vi.unstubAllEnvs();
    },
  );
});
