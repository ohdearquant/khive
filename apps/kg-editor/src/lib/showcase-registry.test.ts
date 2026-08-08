import { describe, expect, it } from "vitest";

import {
  isAllowedShowcaseAsset,
  normalizeRepositoryUrl,
  resolveShowcaseRepository,
} from "@/lib/showcase-registry";

describe("showcase registry", () => {
  it.each([
    "https://github.com/ohdearquant/khive",
    "https://github.com/ohdearquant/khive/",
    "https://github.com/ohdearquant/khive.git",
    "http://github.com/ohdearquant/khive.git",
    "https://www.github.com/ohdearquant/khive?tab=readme#readme",
  ])("resolves a curated alias without fetching the submitted URL: %s", (input) => {
    const result = resolveShowcaseRepository(input);

    expect(result.status).toBe("hit");
    if (result.status === "hit") {
      expect(result.entry.assetPath).toBe("/showcase/khive-repo-v1-khive.json");
      expect(result.normalizedUrl).toBe("https://github.com/ohdearquant/khive");
    }
  });

  it("distinguishes a valid URL outside the curated set from invalid input", () => {
    expect(resolveShowcaseRepository("https://github.com/example/not-curated")).toEqual({
      status: "miss",
      normalizedUrl: "https://github.com/example/not-curated",
    });
    expect(resolveShowcaseRepository("https://github.com:444/ohdearquant/khive")).toMatchObject({
      status: "miss",
    });
    expect(resolveShowcaseRepository("github.com/example/repo")).toEqual({
      status: "invalid",
      reason: "Enter a complete http or https repository URL.",
    });
  });

  it("rejects credentials, non-http protocols, and path traversal", () => {
    expect(normalizeRepositoryUrl("https://token@github.com/example/repo").ok).toBe(false);
    expect(normalizeRepositoryUrl("file:///tmp/repo").ok).toBe(false);
    expect(normalizeRepositoryUrl("https://github.com/example/%2E%2E/repo").ok).toBe(false);
  });

  it("allows only registry-owned same-origin static assets", () => {
    expect(isAllowedShowcaseAsset("/showcase/khive-repo-v1-khive.json")).toBe(true);
    expect(isAllowedShowcaseAsset("https://example.com/showcase.json")).toBe(false);
    expect(isAllowedShowcaseAsset("/showcase/../../secrets.json")).toBe(false);
    expect(isAllowedShowcaseAsset("/showcase/unknown.json")).toBe(false);
  });
});
