import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";

import {
  loadStaticShowcaseBundle,
  REPO_BUNDLE_MAX_BYTES,
  REPO_BUNDLE_MAX_MIB,
} from "@/lib/adapters/static-showcase-source";
import { SHOWCASE_REGISTRY } from "@/lib/showcase-registry";

const golden = readFileSync(resolve(process.cwd(), "../../docs/schemas/examples/khive-repo-v1-khive.json"));

function response(bytes: Uint8Array, declaredLength = bytes.byteLength) {
  return {
    ok: true,
    status: 200,
    headers: new Headers({ "content-length": String(declaredLength) }),
    arrayBuffer: () => Promise.resolve(Uint8Array.from(bytes).buffer as ArrayBuffer),
  };
}

describe("static showcase source", () => {
  it("fetches only the registry-owned same-origin golden and validates it", async () => {
    const fetchBundle = vi.fn(async () => response(golden));

    const bundle = await loadStaticShowcaseBundle(SHOWCASE_REGISTRY[0], fetchBundle);

    expect(bundle.schema_version).toBe("khive.repo.v1");
    expect(fetchBundle).toHaveBeenCalledWith(
      "/showcase/khive-repo-v1-khive.json",
      { cache: "force-cache", credentials: "same-origin", redirect: "error" },
    );
  });

  it("refuses an asset path not owned by the curated registry before fetch", async () => {
    const fetchBundle = vi.fn();

    await expect(loadStaticShowcaseBundle({
      ...SHOWCASE_REGISTRY[0],
      assetPath: "https://example.com/repository.json",
    }, fetchBundle)).rejects.toThrow(/unapproved showcase asset/i);
    expect(fetchBundle).not.toHaveBeenCalled();
  });

  it("rejects an oversized response from its declared length before reading bytes", async () => {
    const arrayBuffer = vi.fn();
    const fetchBundle = vi.fn(async () => ({
      ...response(new Uint8Array()),
      headers: new Headers({ "content-length": String(REPO_BUNDLE_MAX_BYTES + 1) }),
      arrayBuffer,
    }));

    await expect(loadStaticShowcaseBundle(SHOWCASE_REGISTRY[0], fetchBundle)).rejects.toThrow(new RegExp(`${REPO_BUNDLE_MAX_MIB} MiB`, "i"));
    expect(arrayBuffer).not.toHaveBeenCalled();
  });
});
