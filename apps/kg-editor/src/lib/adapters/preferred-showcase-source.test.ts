import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";

import {
  DB_SNAPSHOT_TIMEOUT_MS,
  loadPreferredShowcaseBundle,
  readOperatorShowcaseAccessToken,
  SHOWCASE_ACCESS_TOKEN_STORAGE_KEY,
  ShowcaseAnalysisNotFoundError,
} from "@/lib/adapters/preferred-showcase-source";
import { SHOWCASE_REGISTRY } from "@/lib/showcase-registry";

const golden = readFileSync(
  resolve(
    process.cwd(),
    "../../docs/schemas/examples/khive-repo-v1-khive.json",
  ),
);

function response(
  bytes: Uint8Array,
  status: number,
  headers: Record<string, string> = {},
) {
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: new Headers({
      "content-length": String(bytes.byteLength),
      ...headers,
    }),
    arrayBuffer: () =>
      Promise.resolve(Uint8Array.from(bytes).buffer as ArrayBuffer),
  };
}

describe("preferred showcase source", () => {
  const configuredStaticEntry = {
    ...SHOWCASE_REGISTRY[0],
    analysisId: "khive",
  };

  it("prefers the configured khive DB snapshot and verifies its provenance", async () => {
    const fetchBundle = vi.fn(async () =>
      response(golden, 200, {
        "x-khive-analysis-id": "khive",
        "x-khive-analysis-source": "khive-db-snapshot",
      })
    );

    const result = await loadPreferredShowcaseBundle(
      configuredStaticEntry,
      fetchBundle,
    );

    expect(result.source).toBe("khive-db-snapshot");
    expect(result.bundle.schema_version).toBe("khive.repo.v1");
    expect(fetchBundle).toHaveBeenCalledOnce();
    expect(fetchBundle).toHaveBeenCalledWith("/api/showcase/analyses/khive", {
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
      signal: expect.any(AbortSignal),
    });
  });

  it("sends the operator bearer token to the protected snapshot route when supplied", async () => {
    const fetchBundle = vi.fn(async () =>
      response(golden, 200, {
        "x-khive-analysis-id": "khive",
        "x-khive-analysis-source": "khive-db-snapshot",
      })
    );

    const result = await loadPreferredShowcaseBundle(
      configuredStaticEntry,
      fetchBundle,
      { accessToken: "  operator-secret  " },
    );

    expect(result.source).toBe("khive-db-snapshot");
    expect(fetchBundle).toHaveBeenCalledWith("/api/showcase/analyses/khive", {
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
      signal: expect.any(AbortSignal),
      headers: { authorization: "Bearer operator-secret" },
    });
  });

  it("sends no Authorization header when the operator token is blank", async () => {
    const fetchBundle = vi.fn(async () =>
      response(golden, 200, {
        "x-khive-analysis-id": "khive",
        "x-khive-analysis-source": "khive-db-snapshot",
      })
    );

    await loadPreferredShowcaseBundle(configuredStaticEntry, fetchBundle, {
      accessToken: "   ",
    });

    expect(fetchBundle).toHaveBeenCalledWith("/api/showcase/analyses/khive", {
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
      signal: expect.any(AbortSignal),
    });
  });

  it("reads the operator token from browser session storage", () => {
    window.sessionStorage.setItem(
      SHOWCASE_ACCESS_TOKEN_STORAGE_KEY,
      "session-secret",
    );
    try {
      expect(readOperatorShowcaseAccessToken()).toBe("session-secret");
    } finally {
      window.sessionStorage.removeItem(SHOWCASE_ACCESS_TOKEN_STORAGE_KEY);
    }
    expect(readOperatorShowcaseAccessToken()).toBeNull();
  });

  it("uses the curated asset only when the DB snapshot route is not configured", async () => {
    const fetchBundle = vi.fn(async (input: string) =>
      input.startsWith("/api/")
        ? response(new Uint8Array(), 404)
        : response(golden, 200)
    );

    const result = await loadPreferredShowcaseBundle(
      configuredStaticEntry,
      fetchBundle,
    );

    expect(result.source).toBe("curated-static-fallback");
    expect(fetchBundle).toHaveBeenCalledTimes(2);
    expect(fetchBundle.mock.calls[1]?.[0]).toBe(
      "/showcase/khive-repo-v1-khive.json",
    );
  });

  it("does not hide an invalid or unavailable DB snapshot behind stale static data", async () => {
    const fetchBundle = vi.fn(async () => response(new Uint8Array(), 503));

    await expect(
      loadPreferredShowcaseBundle(configuredStaticEntry, fetchBundle),
    ).rejects.toThrow(/database snapshot.*503/i);
    expect(fetchBundle).toHaveBeenCalledOnce();
  });

  it("rejects a successful response without the configured snapshot identity", async () => {
    const fetchBundle = vi.fn(async () =>
      response(golden, 200, {
        "x-khive-analysis-id": "other",
        "x-khive-analysis-source": "khive-db-snapshot",
      })
    );

    await expect(
      loadPreferredShowcaseBundle(configuredStaticEntry, fetchBundle),
    ).rejects.toThrow(/provenance/i);
  });

  it("rejects a valid snapshot for a different repository", async () => {
    const wrongRepository = JSON.parse(golden.toString("utf8"));
    wrongRepository.meta.repository.canonical_url =
      "https://github.com/example/not-khive";
    const fetchBundle = vi.fn(async () =>
      response(Buffer.from(JSON.stringify(wrongRepository)), 200, {
        "x-khive-analysis-id": "khive",
        "x-khive-analysis-source": "khive-db-snapshot",
      })
    );

    await expect(
      loadPreferredShowcaseBundle(configuredStaticEntry, fetchBundle),
    ).rejects.toThrow(/repository identity/i);
    expect(fetchBundle).toHaveBeenCalledOnce();
  });

  it("loads a static-only entry without probing the analysis API", async () => {
    const fetchBundle = vi.fn(async () => response(golden, 200));

    const result = await loadPreferredShowcaseBundle(
      {
        ...SHOWCASE_REGISTRY[0],
        analysisId: undefined,
      },
      fetchBundle,
    );

    expect(result.source).toBe("curated-static-fallback");
    expect(fetchBundle).toHaveBeenCalledOnce();
    expect(fetchBundle).toHaveBeenCalledWith(
      "/showcase/khive-repo-v1-khive.json",
      expect.any(Object),
    );
  });

  it("returns a miss for a dynamic-only analysis 404", async () => {
    const entry = {
      id: "analysis:dynamic-only",
      canonicalUrl: "https://github.com/example/dynamic-only",
      aliases: ["https://github.com/example/dynamic-only"],
      analysisId: "dynamic-only",
    };
    const fetchBundle = vi.fn(async () => response(new Uint8Array(), 404));

    await expect(loadPreferredShowcaseBundle(entry, fetchBundle)).rejects
      .toBeInstanceOf(ShowcaseAnalysisNotFoundError);
    expect(fetchBundle).toHaveBeenCalledOnce();
    expect(fetchBundle).toHaveBeenCalledWith(
      "/api/showcase/analyses/dynamic-only",
      expect.any(Object),
    );
  });

  it("reports a hard failure when the DB snapshot request never settles", async () => {
    vi.useFakeTimers();
    try {
      const fetchBundle = vi.fn(() => new Promise<never>(() => {}));

      const resultPromise = loadPreferredShowcaseBundle(
        configuredStaticEntry,
        fetchBundle,
      );
      const assertion = expect(resultPromise).rejects.toThrow(
        /did not settle/i,
      );
      await vi.advanceTimersByTimeAsync(DB_SNAPSHOT_TIMEOUT_MS);
      await assertion;
    } finally {
      vi.useRealTimers();
    }
  });

  it("reports a hard failure when the DB snapshot response body never completes", async () => {
    vi.useFakeTimers();
    try {
      const fetchBundle = vi.fn(() =>
        Promise.resolve({
          ok: true,
          status: 200,
          headers: new Headers({
            "x-khive-analysis-id": "khive",
            "x-khive-analysis-source": "khive-db-snapshot",
          }),
          arrayBuffer: () => new Promise<ArrayBuffer>(() => {}),
        })
      );

      const resultPromise = loadPreferredShowcaseBundle(
        configuredStaticEntry,
        fetchBundle,
      );
      const assertion = expect(resultPromise).rejects.toThrow(
        /did not settle/i,
      );
      await vi.advanceTimersByTimeAsync(DB_SNAPSHOT_TIMEOUT_MS);
      await assertion;
    } finally {
      vi.useRealTimers();
    }
  });

  it("never substitutes stale static data for a failing DB snapshot", async () => {
    const failureModes: Array<() => Promise<ReturnType<typeof response>>> = [
      () => Promise.reject(new TypeError("network down")),
      () => Promise.resolve(response(new Uint8Array(), 500)),
      () =>
        Promise.resolve(
          response(new TextEncoder().encode("{"), 200, {
            "x-khive-analysis-id": "khive",
            "x-khive-analysis-source": "khive-db-snapshot",
          }),
        ),
    ];

    for (const failureMode of failureModes) {
      const fetchBundle = vi.fn(async (input: string) =>
        input.startsWith("/api/") ? failureMode() : response(golden, 200)
      );

      await expect(
        loadPreferredShowcaseBundle(configuredStaticEntry, fetchBundle),
      ).rejects.toThrow();
      expect(fetchBundle).toHaveBeenCalledOnce();
    }
  });
});
