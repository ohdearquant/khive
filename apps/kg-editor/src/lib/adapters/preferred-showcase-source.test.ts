import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";

import {
  DB_SNAPSHOT_TIMEOUT_MS,
  loadPreferredShowcaseBundle,
  readOperatorShowcaseAccessToken,
  SHOWCASE_ACCESS_TOKEN_STORAGE_KEY,
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
  it("prefers the configured khive DB snapshot and verifies its provenance", async () => {
    const fetchBundle = vi.fn(async () =>
      response(golden, 200, {
        "x-khive-analysis-id": "khive",
        "x-khive-analysis-source": "khive-db-snapshot",
      })
    );

    const result = await loadPreferredShowcaseBundle(
      SHOWCASE_REGISTRY[0],
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
      SHOWCASE_REGISTRY[0],
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

    await loadPreferredShowcaseBundle(SHOWCASE_REGISTRY[0], fetchBundle, {
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
      SHOWCASE_REGISTRY[0],
      fetchBundle,
    );

    expect(result.source).toBe("curated-static-fallback");
    expect(fetchBundle).toHaveBeenCalledTimes(2);
    expect(fetchBundle.mock.calls[1]?.[0]).toBe(
      "/showcase/khive-repo-v1-khive.json",
    );
  });

  it("falls back to the static asset when the DB snapshot service errors", async () => {
    const fetchBundle = vi.fn(async (input: string) =>
      input.startsWith("/api/")
        ? response(new Uint8Array(), 503)
        : response(golden, 200)
    );

    const result = await loadPreferredShowcaseBundle(
      SHOWCASE_REGISTRY[0],
      fetchBundle,
    );

    expect(result.source).toBe("curated-static-fallback");
    expect(result.bundle.schema_version).toBe("khive.repo.v1");
    expect(fetchBundle).toHaveBeenCalledTimes(2);
  });

  it("falls back to the static asset when the fetch rejects at the network level", async () => {
    const fetchBundle = vi.fn(async (input: string) => {
      if (input.startsWith("/api/")) {
        throw new TypeError("Failed to fetch");
      }
      return response(golden, 200);
    });

    const result = await loadPreferredShowcaseBundle(
      SHOWCASE_REGISTRY[0],
      fetchBundle,
    );

    expect(result.source).toBe("curated-static-fallback");
    expect(result.bundle.schema_version).toBe("khive.repo.v1");
    expect(fetchBundle).toHaveBeenCalledTimes(2);
  });

  it("falls back to the static asset when the DB snapshot body is invalid", async () => {
    const invalid = new TextEncoder().encode("not json");
    const fetchBundle = vi.fn(async (input: string) =>
      input.startsWith("/api/")
        ? response(invalid, 200, {
          "x-khive-analysis-id": "khive",
          "x-khive-analysis-source": "khive-db-snapshot",
        })
        : response(golden, 200)
    );

    const result = await loadPreferredShowcaseBundle(
      SHOWCASE_REGISTRY[0],
      fetchBundle,
    );

    expect(result.source).toBe("curated-static-fallback");
    expect(result.bundle.schema_version).toBe("khive.repo.v1");
    expect(fetchBundle).toHaveBeenCalledTimes(2);
  });

  it("falls back to the static asset when the DB snapshot body exceeds the browser limit", async () => {
    const oversized = new Uint8Array(9 * 1024 * 1024);
    const fetchBundle = vi.fn(async (input: string) =>
      input.startsWith("/api/")
        ? response(oversized, 200, {
          "x-khive-analysis-id": "khive",
          "x-khive-analysis-source": "khive-db-snapshot",
        })
        : response(golden, 200)
    );

    const result = await loadPreferredShowcaseBundle(
      SHOWCASE_REGISTRY[0],
      fetchBundle,
    );

    expect(result.source).toBe("curated-static-fallback");
    expect(result.bundle.schema_version).toBe("khive.repo.v1");
    expect(fetchBundle).toHaveBeenCalledTimes(2);
  });

  it("falls back to the static asset when the DB snapshot response lacks the configured provenance identity", async () => {
    const fetchBundle = vi.fn(async (input: string) =>
      input.startsWith("/api/")
        ? response(golden, 200, {
          "x-khive-analysis-id": "other",
          "x-khive-analysis-source": "khive-db-snapshot",
        })
        : response(golden, 200)
    );

    const result = await loadPreferredShowcaseBundle(
      SHOWCASE_REGISTRY[0],
      fetchBundle,
    );

    expect(result.source).toBe("curated-static-fallback");
    expect(result.bundle.schema_version).toBe("khive.repo.v1");
    expect(fetchBundle).toHaveBeenCalledTimes(2);
  });

  it("falls back to the static asset when a valid snapshot is for a different repository", async () => {
    const wrongRepository = JSON.parse(golden.toString("utf8"));
    wrongRepository.meta.repository.canonical_url =
      "https://github.com/example/not-khive";
    const fetchBundle = vi.fn(async (input: string) =>
      input.startsWith("/api/")
        ? response(Buffer.from(JSON.stringify(wrongRepository)), 200, {
          "x-khive-analysis-id": "khive",
          "x-khive-analysis-source": "khive-db-snapshot",
        })
        : response(golden, 200)
    );

    const result = await loadPreferredShowcaseBundle(
      SHOWCASE_REGISTRY[0],
      fetchBundle,
    );

    expect(result.source).toBe("curated-static-fallback");
    expect(result.bundle.schema_version).toBe("khive.repo.v1");
    expect(fetchBundle).toHaveBeenCalledTimes(2);
  });

  it("falls back to the static asset when the DB snapshot request never settles", async () => {
    vi.useFakeTimers();
    try {
      const fetchBundle = vi.fn((input: string) => {
        if (input.startsWith("/api/")) {
          return new Promise<never>(() => {});
        }
        return Promise.resolve(response(golden, 200));
      });

      const resultPromise = loadPreferredShowcaseBundle(
        SHOWCASE_REGISTRY[0],
        fetchBundle,
      );
      await vi.advanceTimersByTimeAsync(DB_SNAPSHOT_TIMEOUT_MS);
      const result = await resultPromise;

      expect(result.source).toBe("curated-static-fallback");
      expect(result.bundle.schema_version).toBe("khive.repo.v1");
    } finally {
      vi.useRealTimers();
    }
  });

  it("falls back to the static asset when the DB snapshot response body never completes", async () => {
    vi.useFakeTimers();
    try {
      const fetchBundle = vi.fn((input: string) => {
        if (input.startsWith("/api/")) {
          return Promise.resolve({
            ok: true,
            status: 200,
            headers: new Headers({
              "x-khive-analysis-id": "khive",
              "x-khive-analysis-source": "khive-db-snapshot",
            }),
            arrayBuffer: () => new Promise<ArrayBuffer>(() => {}),
          });
        }
        return Promise.resolve(response(golden, 200));
      });

      const resultPromise = loadPreferredShowcaseBundle(
        SHOWCASE_REGISTRY[0],
        fetchBundle,
      );
      await vi.advanceTimersByTimeAsync(DB_SNAPSHOT_TIMEOUT_MS);
      const result = await resultPromise;

      expect(result.source).toBe("curated-static-fallback");
      expect(result.bundle.schema_version).toBe("khive.repo.v1");
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not surface an unhandled rejection or thrown error when every DB snapshot failure mode falls back", async () => {
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
        loadPreferredShowcaseBundle(SHOWCASE_REGISTRY[0], fetchBundle),
      ).resolves.toMatchObject({ source: "curated-static-fallback" });
    }
  });
});
