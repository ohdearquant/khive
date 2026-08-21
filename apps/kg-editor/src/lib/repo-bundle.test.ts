import Ajv2020 from "ajv/dist/2020";
import addFormats from "ajv-formats";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

import { parseRepoBundle, repoBundleSchema } from "@/lib/repo-bundle";

const goldenPath = resolve(process.cwd(), "../../docs/schemas/examples/khive-repo-v1-khive.json");
const schemaPath = resolve(process.cwd(), "../../docs/schemas/khive-repo-v1.schema.json");
const browserAssetPath = resolve(process.cwd(), "public/showcase/khive-repo-v1-khive.json");

function goldenValue(): unknown {
  return JSON.parse(readFileSync(goldenPath, "utf8"));
}

describe("khive.repo.v1 browser contract", () => {
  it("consumes the Rust golden with a closed wire model", () => {
    const parsed = parseRepoBundle(goldenValue());

    expect(parsed.schema_version).toBe("khive.repo.v1");
    expect(parsed.meta.snapshot.head_sha).toBe("c2979d2443738a075e55a170c772d1dc86cf0f91");
    expect(Object.keys(parsed.capability.views)).toHaveLength(10);
    expect(parsed.graph.functions.items).toEqual([]);
    expect(parsed.graph.datatypes.items).toEqual([]);
    expect(parsed.graph.interfaces.items).toEqual([]);
  });

  it("rejects unknown fields rather than silently drifting", () => {
    const value = goldenValue() as Record<string, unknown>;
    value.browser_only_guess = true;

    expect(repoBundleSchema.safeParse(value).success).toBe(false);
  });

  it("rejects disagreement between view and aggregate availability", () => {
    const value = goldenValue() as {
      capability: { views: { hidden_coupling: { status: string; unavailable_reason?: string } } };
    };
    value.capability.views.hidden_coupling.status = "unavailable";
    value.capability.views.hidden_coupling.unavailable_reason =
      "analysis was not produced";

    expect(repoBundleSchema.safeParse(value).success).toBe(false);
  });

  it("rejects duplicate module source paths that cannot form a stable investigation link", () => {
    const value = goldenValue() as {
      graph: { modules: { items: Array<{ id: string; source_path: string }> } };
    };
    expect(value.graph.modules.items.length).toBeGreaterThan(1);
    value.graph.modules.items[1].source_path =
      value.graph.modules.items[0].source_path;

    expect(repoBundleSchema.safeParse(value).success).toBe(false);
  });

  it.each([
    "",
    "/absolute.rs",
    "crates/../outside.rs",
    "crates\\windows.rs",
    `crates/${"a".repeat(1_025)}`,
    "crates/control\u0000.rs",
  ])("rejects a non-addressable module source path %j", (sourcePath) => {
    const value = goldenValue() as {
      graph: { modules: { items: Array<{ source_path: string }> } };
    };
    value.graph.modules.items[0].source_path = sourcePath;

    expect(repoBundleSchema.safeParse(value).success).toBe(false);
  });

  it("rejects a canonical repository URL that cannot be shared publicly", () => {
    const value = goldenValue() as {
      meta: { repository: { canonical_url: string } };
    };
    value.meta.repository.canonical_url = "file:///private/repository";

    expect(repoBundleSchema.safeParse(value).success).toBe(false);
  });

  it("accepts repository ownership when only the module join is unavailable", () => {
    const value = goldenValue() as {
      capability: { views: { ownership: { status: string; unavailable_reason?: string } } };
      aggregates: { ownership: { modules: { items: unknown[]; total_count: unknown; next_cursor?: string | null; truncated: boolean; disclosure: { status: string; reason?: string | null } } } };
    };
    value.capability.views.ownership.status = "unavailable";
    value.capability.views.ownership.unavailable_reason = "module join was not complete";
    value.aggregates.ownership.modules.items = [];
    value.aggregates.ownership.modules.total_count = {
      status: "unavailable",
      reason: "module join was not complete",
    };
    value.aggregates.ownership.modules.next_cursor = null;
    value.aggregates.ownership.modules.truncated = false;
    value.aggregates.ownership.modules.disclosure = {
      status: "unavailable",
      reason: "module join was not complete",
    };

    expect(repoBundleSchema.safeParse(value).success).toBe(true);
  });

  it("also validates the golden against the normative JSON Schema", () => {
    const ajv = new Ajv2020({ allErrors: true, strict: false });
    addFormats(ajv);
    // schemars preserves Rust numeric wire names as annotations. The schema's
    // integer/minimum/maximum keywords remain the normative validators.
    for (const format of ["uint32", "uint64", "double"]) {
      ajv.addFormat(format, { type: "number", validate: () => true });
    }
    const validate = ajv.compile(JSON.parse(readFileSync(schemaPath, "utf8")));

    expect(validate(goldenValue()), JSON.stringify(validate.errors, null, 2)).toBe(true);
  });

  it("ships the exact canonical bytes consumed by Rust and CI to the browser", () => {
    expect(readFileSync(browserAssetPath).equals(readFileSync(goldenPath))).toBe(true);
  });

  it("keeps all four join analyses tagged as joins", () => {
    const bundle = parseRepoBundle(goldenValue());
    const joins = [
      "history_structure_navigation",
      "hotspot_quadrant",
      "hidden_coupling",
      "ownership",
    ] as const;

    for (const view of joins) expect(bundle.capability.views[view].join).toBe("join");
    expect(bundle.capability.views.cadence_timeline.granularity).toBe("repository");
    expect(bundle.capability.views.cadence_timeline.join).toBe("history_only");
    expect(bundle.capability.views.scorecard.join).toBe("field_tagged");
  });

  it("keeps cadence source absence independently typed instead of zero filling", () => {
    const cadence = parseRepoBundle(goldenValue()).aggregates.cadence_timeline;

    expect(cadence.commits.disclosure.status).toBe("complete");
    for (const page of [
      cadence.issues_opened,
      cadence.issues_closed,
      cadence.pull_requests_opened,
      cadence.pull_requests_merged,
    ]) {
      expect(page.items).toEqual([]);
      expect(page.disclosure.status).toBe("unavailable");
      expect(page.total_count.status).toBe("unavailable");
    }
  });

  it("rejects a derived edge whose derivation evidence was erased", () => {
    const value = goldenValue() as {
      graph: { commit_module_edges: { items: Array<{ derivation: unknown }> } };
    };
    expect(value.graph.commit_module_edges.items.length).toBeGreaterThan(0);
    value.graph.commit_module_edges.items[0].derivation = null;

    expect(repoBundleSchema.safeParse(value).success).toBe(false);
  });
});
