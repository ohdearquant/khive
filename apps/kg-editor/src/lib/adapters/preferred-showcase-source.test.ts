import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";

import { loadPreferredShowcaseBundle } from "@/lib/adapters/preferred-showcase-source";
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
    });
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

  it("does not hide an invalid or unavailable DB snapshot behind stale static data", async () => {
    const fetchBundle = vi.fn(async () => response(new Uint8Array(), 503));

    await expect(
      loadPreferredShowcaseBundle(SHOWCASE_REGISTRY[0], fetchBundle),
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
      loadPreferredShowcaseBundle(SHOWCASE_REGISTRY[0], fetchBundle),
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
      loadPreferredShowcaseBundle(SHOWCASE_REGISTRY[0], fetchBundle),
    ).rejects.toThrow(/repository identity/i);
    expect(fetchBundle).toHaveBeenCalledOnce();
  });
});
