import { describe, expect, it } from "vitest";

import { fixtureReviewSource } from "@/lib/adapters/fixture-review-source";

describe("fixture review source", () => {
  it("exposes a schema-validated, explicitly read-only source", async () => {
    const bundle = await fixtureReviewSource.load();

    expect(fixtureReviewSource.capabilities).toEqual({
      gitReads: false,
      khiveReads: false,
      githubWrites: false,
      wasm: false,
    });
    expect(bundle.capability.no_writes).toBe(true);
    expect(bundle.capability.source).toBe("fixture");
  });
});
