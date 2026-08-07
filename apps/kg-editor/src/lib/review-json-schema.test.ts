import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { describe, expect, it } from "vitest";
import { z } from "zod";

import { buildReviewJsonSchema } from "../../scripts/review-json-schema.mjs";
import reviewSchema from "../../../../docs/schemas/khive-review-v1.schema.json";
import changesetGolden from "../../../../docs/schemas/examples/khive-review-v1-changeset.json";
import { atlasReviewFixture } from "@/lib/fixtures/atlas-review";
import { parseReviewInput, reviewInputSchema } from "@/lib/review-bundle";

describe("normative khive.review.v1 JSON Schema", () => {
  const ajv = new Ajv2020({ allErrors: true, strict: false });
  addFormats(ajv);
  const validate = ajv.compile(reviewSchema);

  it("is generated from the same closed Zod wire model", () => {
    expect(reviewSchema).toEqual(buildReviewJsonSchema(reviewInputSchema, z));
  });

  it("accepts the bounded pull-request fixture", () => {
    expect(validate(atlasReviewFixture), JSON.stringify(validate.errors)).toBe(true);
  });

  it("accepts the exact Rust-produced shared changeset golden", () => {
    expect(validate(changesetGolden), JSON.stringify(validate.errors)).toBe(true);
    expect(() => parseReviewInput(changesetGolden)).not.toThrow();
  });

  it("rejects fields and claims outside the first-slice contract", () => {
    expect(validate({ ...changesetGolden, repository: { invented: true } })).toBe(false);

    const missingLabel = {
      ...atlasReviewFixture,
      capability: { ...atlasReviewFixture.capability, label: undefined },
    };
    expect(validate(missingLabel)).toBe(false);
    expect(() => parseReviewInput(missingLabel)).toThrow();

    const unratifiedVerifiedHash = {
      ...atlasReviewFixture,
      snapshot_identity: {
        ...atlasReviewFixture.snapshot_identity,
        hash_status: "verified",
      },
    };
    expect(validate(unratifiedVerifiedHash)).toBe(false);
    expect(() => parseReviewInput(unratifiedVerifiedHash)).toThrow();

    const unavailableWithHashes = {
      ...atlasReviewFixture,
      snapshot_identity: {
        ...atlasReviewFixture.snapshot_identity,
        hash_status: "unavailable",
      },
    };
    expect(validate(unavailableWithHashes)).toBe(false);
    expect(() => parseReviewInput(unavailableWithHashes)).toThrow();
  });
});
