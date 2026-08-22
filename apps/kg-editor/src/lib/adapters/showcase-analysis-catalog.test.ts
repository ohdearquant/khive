import { describe, expect, it, vi } from "vitest";

import { SHOWCASE_ACCESS_TOKEN_STORAGE_KEY } from "@/lib/adapters/preferred-showcase-source";
import {
  loadShowcaseAnalysisCatalog,
  mergeShowcaseRegistry,
  parseShowcaseAnalysisCatalog,
  type ShowcaseCatalogFetch,
  SHOWCASE_CATALOG_MAX_BYTES,
  SHOWCASE_CATALOG_MAX_ENTRIES,
  SHOWCASE_CATALOG_TIMEOUT_MS,
} from "@/lib/adapters/showcase-analysis-catalog";
import type { ShowcaseRegistryEntry } from "@/lib/showcase-registry";

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
    body: null,
  };
}

function chunkedCatalogResponse(chunks: readonly Uint8Array<ArrayBuffer>[]) {
  const stream = new ReadableStream<Uint8Array<ArrayBuffer>>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(chunk);
      controller.close();
    },
  });
  const arrayBuffer = vi.fn(async () => {
    throw new Error("arrayBuffer() should not be used when a body stream is available");
  });
  return {
    response: {
      ok: true,
      status: 200,
      headers: new Headers(),
      arrayBuffer,
      body: stream,
    },
    arrayBuffer,
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
        signal: expect.any(AbortSignal),
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
      signal: expect.any(AbortSignal),
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

  it("rejects an oversized chunked body with no Content-Length before full materialization", async () => {
    const chunk = new Uint8Array(64 * 1024).fill(0x20);
    const { response, arrayBuffer } = chunkedCatalogResponse([
      chunk,
      chunk,
      chunk,
      chunk,
      chunk,
    ]);
    const fetchCatalog = vi.fn(async () => response);

    await expect(loadShowcaseAnalysisCatalog(fetchCatalog)).resolves.toEqual({
      status: "degraded",
      entries: [],
      message: expect.stringMatching(/catalog.*unavailable/i),
    });
    expect(arrayBuffer).not.toHaveBeenCalled();
  });

  it("resolves to degraded within the timeout bound when the catalog fetch never settles", async () => {
    vi.useFakeTimers();
    try {
      const fetchCatalog = vi.fn(() => new Promise<never>(() => {}));
      const resultPromise = loadShowcaseAnalysisCatalog(fetchCatalog);

      await vi.advanceTimersByTimeAsync(SHOWCASE_CATALOG_TIMEOUT_MS);

      await expect(resultPromise).resolves.toEqual({
        status: "degraded",
        entries: [],
        message: expect.stringMatching(/catalog.*unavailable/i),
      });
    } finally {
      vi.useRealTimers();
    }
  });

  it("never starts reading the body once the deadline has already fired, even when the fetch resolves late with a body that never completes", async () => {
    vi.useFakeTimers();
    try {
      let resolveFetch: (value: unknown) => void = () => {};
      const fetchPromise = new Promise((resolve) => {
        resolveFetch = resolve;
      });
      const fetchCatalog = vi.fn(
        () => fetchPromise,
      ) as unknown as ShowcaseCatalogFetch;
      const resultPromise = loadShowcaseAnalysisCatalog(fetchCatalog);

      await vi.advanceTimersByTimeAsync(SHOWCASE_CATALOG_TIMEOUT_MS);
      await expect(resultPromise).resolves.toEqual({
        status: "degraded",
        entries: [],
        message: expect.stringMatching(/catalog.*unavailable/i),
      });

      const read = vi.fn(() => new Promise(() => {}));
      const cancel = vi.fn(async () => {});
      const getReader = vi.fn(() => ({
        read,
        cancel,
        releaseLock: vi.fn(),
      }));
      resolveFetch({
        ok: true,
        status: 200,
        headers: new Headers(),
        arrayBuffer: vi.fn(async () => {
          throw new Error(
            "arrayBuffer() should not be used when a body stream is available",
          );
        }),
        body: { getReader } as unknown as ReadableStream<
          Uint8Array<ArrayBuffer>
        >,
      });
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();

      expect(getReader).not.toHaveBeenCalled();
      expect(read).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("attaches a handler to a rejecting reader.cancel() triggered by an in-flight abort, and still settles degraded", async () => {
    vi.useFakeTimers();
    try {
      const read = vi.fn(() => new Promise(() => {}));
      let cancelResultHandled = false;
      const cancel = vi.fn(() => ({
        catch: (onRejected?: (reason: unknown) => unknown) => {
          cancelResultHandled = true;
          return Promise.resolve().then(() =>
            onRejected?.(new Error("cancel failed"))
          );
        },
        then: (
          _onFulfilled?: (value: unknown) => unknown,
          onRejected?: (reason: unknown) => unknown,
        ) => {
          cancelResultHandled = true;
          return Promise.resolve().then(() =>
            onRejected?.(new Error("cancel failed"))
          );
        },
      }));
      const getReader = vi.fn(() => ({
        read,
        cancel,
        releaseLock: vi.fn(),
      }));
      const response = {
        ok: true,
        status: 200,
        headers: new Headers(),
        arrayBuffer: vi.fn(async () => {
          throw new Error(
            "arrayBuffer() should not be used when a body stream is available",
          );
        }),
        body: { getReader } as unknown as ReadableStream<
          Uint8Array<ArrayBuffer>
        >,
      };
      const fetchCatalog = vi.fn(
        async () => response,
      ) as unknown as ShowcaseCatalogFetch;

      const resultPromise = loadShowcaseAnalysisCatalog(fetchCatalog);
      await vi.advanceTimersByTimeAsync(SHOWCASE_CATALOG_TIMEOUT_MS);

      await expect(resultPromise).resolves.toEqual({
        status: "degraded",
        entries: [],
        message: expect.stringMatching(/catalog.*unavailable/i),
      });

      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();

      expect(cancel).toHaveBeenCalled();
      expect(cancelResultHandled).toBe(true);
    } finally {
      vi.useRealTimers();
    }
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

  it("removes legacy analysis IDs when the catalog does not configure them", () => {
    expect(mergeShowcaseRegistry([], [staticEntry])).toEqual([
      {
        ...staticEntry,
        analysisId: undefined,
      },
    ]);
  });
});
