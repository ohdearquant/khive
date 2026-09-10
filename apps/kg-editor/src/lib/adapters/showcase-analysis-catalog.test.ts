import { describe, expect, it, vi } from "vitest";

import { SHOWCASE_ACCESS_TOKEN_STORAGE_KEY } from "@/lib/adapters/preferred-showcase-source";
import {
  loadShowcaseAnalysisCatalog,
  mergeShowcaseRegistry,
  parseShowcaseAnalysisCatalog,
  SHOWCASE_CATALOG_MAX_BYTES,
  SHOWCASE_CATALOG_MAX_ENTRIES,
} from "@/lib/adapters/showcase-analysis-catalog";
import {
  resolveShowcaseRepository,
  type ShowcaseRegistryEntry,
} from "@/lib/showcase-registry";

const staticEntry: ShowcaseRegistryEntry = {
  id: "github.com/example/repository",
  canonicalUrl: "https://github.com/example/repository",
  aliases: ["http://github.com/example/repository.git"],
  assetPath: "/showcase/example.json",
  analysisId: "legacy-static-id",
};

function catalogResponse(
  value: unknown,
  status = 200,
  headers: Record<string, string> = {},
) {
  const bytes = new TextEncoder().encode(JSON.stringify(value));
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: new Headers({
      "content-length": String(bytes.byteLength),
      ...headers,
    }),
    arrayBuffer: () => Promise.resolve(bytes.buffer as ArrayBuffer),
  };
}

describe("showcase analysis catalog", () => {
  it("parses the exact deterministic v1 envelope", () => {
    expect(parseShowcaseAnalysisCatalog({
      schema_version: "khive.showcase.catalog.v1",
      entries: [
        {
          analysis_id: "alpha",
          canonical_url: "https://github.com/example/alpha",
        },
        {
          analysis_id: "zeta",
          canonical_url: "https://github.com/example/zeta",
        },
      ],
    })).toEqual([
      {
        analysis_id: "alpha",
        canonical_url: "https://github.com/example/alpha",
      },
      {
        analysis_id: "zeta",
        canonical_url: "https://github.com/example/zeta",
      },
    ]);
  });

  it.each([
    ["an unknown envelope field", {
      schema_version: "khive.showcase.catalog.v1",
      entries: [],
      root: "/private/analyses",
    }],
    ["an unknown entry field", {
      schema_version: "khive.showcase.catalog.v1",
      entries: [{
        analysis_id: "alpha",
        canonical_url: "https://github.com/example/alpha",
        report_path: "/private/report.json",
      }],
    }],
    ["a non-canonical repository URL", {
      schema_version: "khive.showcase.catalog.v1",
      entries: [{
        analysis_id: "alpha",
        canonical_url: "http://www.github.com/example/alpha.git",
      }],
    }],
    ["entries outside deterministic ID order", {
      schema_version: "khive.showcase.catalog.v1",
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
    }],
  ])("rejects %s", (_name, value) => {
    expect(() => parseShowcaseAnalysisCatalog(value)).toThrow(/catalog/i);
  });

  it("rejects an oversized catalog before accepting any entry", () => {
    const entries = Array.from(
      { length: SHOWCASE_CATALOG_MAX_ENTRIES + 1 },
      (_, index) => ({
        analysis_id: `analysis-${index}`,
        canonical_url: `https://github.com/example/repository-${index}`,
      }),
    );

    expect(() => parseShowcaseAnalysisCatalog({
      schema_version: "khive.showcase.catalog.v1",
      entries,
    })).toThrow(/catalog/i);
  });

  it("rejects an empty successful catalog instead of treating it as unconfigured", () => {
    expect(() => parseShowcaseAnalysisCatalog({
      schema_version: "khive.showcase.catalog.v1",
      entries: [],
    })).toThrow(/catalog/i);
  });

  it.each([
    [
      "analysis ID",
      [
        {
          analysis_id: "same",
          canonical_url: "https://github.com/example/one",
        },
        {
          analysis_id: "same",
          canonical_url: "https://github.com/example/two",
        },
      ],
    ],
    [
      "normalized repository URL",
      [
        {
          analysis_id: "one",
          canonical_url: "https://github.com/example/same",
        },
        {
          analysis_id: "two",
          canonical_url: "https://github.com/example/same",
        },
      ],
    ],
  ])("rejects a duplicate %s", (_name, entries) => {
    expect(() => parseShowcaseAnalysisCatalog({
      schema_version: "khive.showcase.catalog.v1",
      entries,
    })).toThrow(/catalog/i);
  });

  it("treats an unconfigured 404 as static-only", async () => {
    const fetchCatalog = vi.fn(async () => catalogResponse({}, 404));

    await expect(loadShowcaseAnalysisCatalog(fetchCatalog)).resolves.toEqual({
      status: "static-only",
      entries: [],
      message: expect.stringMatching(/not configured/i),
    });
  });

  it.each([
    ["an unavailable response", async () => catalogResponse({}, 503)],
    ["a malformed successful response", async () => catalogResponse({ entries: [] })],
    ["a transport failure", async () => { throw new Error("offline"); }],
  ])("degrades safely for %s", async (_name, fetchCatalog) => {
    await expect(loadShowcaseAnalysisCatalog(fetchCatalog)).resolves.toEqual({
      status: "degraded",
      entries: [],
      message: expect.stringMatching(/catalog.*unavailable/i),
    });
  });

  it("sends the operator bearer token on the catalog fetch when a session token is present", async () => {
    window.sessionStorage.setItem(
      SHOWCASE_ACCESS_TOKEN_STORAGE_KEY,
      "  operator-secret  ",
    );
    try {
      const fetchCatalog = vi.fn(async () => catalogResponse({}, 404));
      await loadShowcaseAnalysisCatalog(fetchCatalog);
      expect(fetchCatalog).toHaveBeenCalledWith("/api/showcase/analyses", {
        cache: "no-store",
        credentials: "same-origin",
        redirect: "error",
        headers: { authorization: "Bearer operator-secret" },
      });
    } finally {
      window.sessionStorage.removeItem(SHOWCASE_ACCESS_TOKEN_STORAGE_KEY);
    }
  });

  it("sends no Authorization header on the catalog fetch when no session token is present", async () => {
    const fetchCatalog = vi.fn(async () => catalogResponse({}, 404));
    await loadShowcaseAnalysisCatalog(fetchCatalog);
    expect(fetchCatalog).toHaveBeenCalledWith("/api/showcase/analyses", {
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
    });
  });

  it("rejects a declared catalog body above its browser limit", async () => {
    const fetchCatalog = vi.fn(async () => catalogResponse(
      {
        schema_version: "khive.showcase.catalog.v1",
        entries: [],
      },
      200,
      { "content-length": String(SHOWCASE_CATALOG_MAX_BYTES + 1) },
    ));

    await expect(loadShowcaseAnalysisCatalog(fetchCatalog)).resolves.toMatchObject({
      status: "degraded",
    });
  });

  it("merges dynamic and static entries by normalized URL", () => {
    const registry = mergeShowcaseRegistry([
      {
        analysis_id: "configured-analysis",
        canonical_url: "https://github.com/example/repository",
      },
      {
        analysis_id: "dynamic-only",
        canonical_url: "https://github.com/example/dynamic",
      },
    ], [staticEntry]);

    expect(registry).toStrictEqual([
      {
        ...staticEntry,
        aliases: [...staticEntry.aliases, "legacy-static-id"],
        analysisId: "configured-analysis",
      },
      {
        id: "analysis:dynamic-only",
        canonicalUrl: "https://github.com/example/dynamic",
        aliases: ["https://github.com/example/dynamic"],
        assetPath: undefined,
        analysisId: "dynamic-only",
      },
    ]);
  });

  it("turns a cleared static analysis ID into a derived lookup alias", () => {
    expect(mergeShowcaseRegistry([], [staticEntry])).toEqual([
      {
        ...staticEntry,
        aliases: [...staticEntry.aliases, "legacy-static-id"],
        analysisId: undefined,
      },
    ]);
  });

  it("lets a catalog entry claiming the legacy static ID win the deep-link lookup", () => {
    const merged = mergeShowcaseRegistry([
      {
        analysis_id: "legacy-static-id",
        canonical_url: "https://github.com/example/other",
      },
    ], [staticEntry]);

    expect(merged[0]?.aliases).not.toContain("legacy-static-id");
    const lookup = resolveShowcaseRepository("legacy-static-id", merged);
    expect(lookup.status).toBe("hit");
    expect(lookup.status === "hit" && lookup.entry.id).toBe(
      "analysis:legacy-static-id",
    );
  });

  it("strips a catalog-claimed ID from another static entry's pre-existing aliases", () => {
    const earlierEntry: ShowcaseRegistryEntry = {
      id: "github.com/example/earlier",
      canonicalUrl: "https://github.com/example/earlier",
      aliases: ["legacy-static-id", "https://github.com/example/earlier"],
      assetPath: "/showcase/earlier.json",
      analysisId: undefined,
    };
    const merged = mergeShowcaseRegistry([
      {
        analysis_id: "legacy-static-id",
        canonical_url: "https://github.com/example/other",
      },
    ], [earlierEntry, staticEntry]);

    expect(merged[0]?.aliases).not.toContain("legacy-static-id");
    const lookup = resolveShowcaseRepository("legacy-static-id", merged);
    expect(lookup.status).toBe("hit");
    expect(lookup.status === "hit" && lookup.entry.id).toBe(
      "analysis:legacy-static-id",
    );
  });

  it("keeps the legacy static ID as an alias when the catalog renames the same repository", () => {
    const merged = mergeShowcaseRegistry([
      {
        analysis_id: "renamed-analysis",
        canonical_url: "https://github.com/example/repository",
      },
    ], [staticEntry]);

    expect(merged).toStrictEqual([
      {
        ...staticEntry,
        aliases: [...staticEntry.aliases, "legacy-static-id"],
        analysisId: "renamed-analysis",
      },
    ]);
    const lookup = resolveShowcaseRepository("legacy-static-id", merged);
    expect(lookup.status).toBe("hit");
    expect(lookup.status === "hit" && lookup.entry.id).toBe(staticEntry.id);
  });
});
