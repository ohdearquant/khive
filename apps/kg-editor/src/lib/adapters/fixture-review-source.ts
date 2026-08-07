import { atlasReviewFixture } from "@/lib/fixtures/atlas-review";
import { parseReviewBundle } from "@/lib/review-bundle";
import type { ReviewSource } from "@/lib/adapters/review-source";

export const fixtureReviewSource: ReviewSource = {
  id: "atlas-review-fixture",
  capabilities: {
    gitReads: false,
    khiveReads: false,
    githubWrites: false,
    wasm: false,
  },
  async load() {
    return parseReviewBundle(atlasReviewFixture);
  },
};
