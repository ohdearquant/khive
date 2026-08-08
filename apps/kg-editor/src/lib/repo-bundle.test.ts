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

type MutableBundle = {
  meta: {
    snapshot: { head_sha: string };
    ingest: {
      code_ingest:
        | { status: "available"; value: Record<string, unknown> }
        | { status: "unavailable"; reason: string };
    };
  };
  graph: Record<"functions" | "datatypes" | "interfaces", unknown>;
};

function availableSymbolPage(items: Array<Record<string, unknown>>, maxItems = 2_000) {
  return {
    items,
    total_count: { status: "available", value: items.length },
    bound: { kind: "all", max_items: maxItems, order: "module_path,name,symbol_id" },
    next_cursor: null,
    truncated: false,
    disclosure: { status: "complete", reason: null },
  };
}

function l2Bundle(symbolsCreated = 0): MutableBundle {
  const value = goldenValue() as MutableBundle;
  const codeIngest = value.meta.ingest.code_ingest;
  if (codeIngest.status !== "available" || !codeIngest.value) {
    throw new Error("golden code provenance must be available");
  }
  codeIngest.value.l2 = {
    source_revision: value.meta.snapshot.head_sha,
    symbols_created: symbolsCreated,
    symbols_updated: 0,
    symbol_dependencies_unresolved: 0,
    symbol_edges_stamped: 0,
    symbol_parse_failures: 0,
  };
  for (const key of ["functions", "datatypes", "interfaces"] as const) {
    value.graph[key] = availableSymbolPage([]);
  }
  return value;
}

function bundleWithFunctionRow(row: Record<string, unknown>): unknown {
  const value = l2Bundle(1);
  value.graph.functions = {
    items: [row],
    total_count: { status: "available", value: 1 },
    bound: { kind: "all", max_items: 1, order: "module_path,name,symbol_id" },
    next_cursor: null,
    truncated: false,
    disclosure: { status: "complete", reason: null },
  };
  return value;
}

const completeFunctionRow = {
  id: "symbol-id",
  module_id: "module-id",
  module_path: "crate::module",
  name: "call_target",
  kind: "function",
  outgoing_call_edge_count: 1,
  outgoing_type_reference_edge_count: 2,
  incoming_implements_edge_count: 3,
};

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

  it("accepts a complete symbol row when L2 provenance is present", () => {
    const result = repoBundleSchema.safeParse(bundleWithFunctionRow(completeFunctionRow));

    expect(result.success).toBe(true);
  });

  it("accepts measured zero pages when L2 provenance is present", () => {
    expect(repoBundleSchema.safeParse(l2Bundle()).success).toBe(true);
  });

  it("requires the exact legacy page shape when L2 provenance is absent", () => {
    const value = goldenValue() as MutableBundle;
    const functions = value.graph.functions as {
      bound: { order: string };
    };
    functions.bound.order = "module_path,name,symbol_id";

    const result = repoBundleSchema.safeParse(value);

    if (result.success) throw new Error("a changed legacy page shape was accepted");
    expect(result.error.issues).toEqual(expect.arrayContaining([
      expect.objectContaining({
        path: ["graph", "functions"],
        message: "symbol page must retain the legacy unavailable shape when L2 provenance is absent",
      }),
    ]));
  });

  it("accepts unavailable code-ingest provenance with exact legacy symbol pages", () => {
    const value = goldenValue() as MutableBundle;
    value.meta.ingest.code_ingest = {
      status: "unavailable",
      reason: "code ingest provenance was not supplied",
    };

    const parsed = parseRepoBundle(value);

    expect(parsed.meta.ingest.code_ingest).toEqual({
      status: "unavailable",
      reason: "code ingest provenance was not supplied",
    });
    for (const key of ["functions", "datatypes", "interfaces"] as const) {
      expect(parsed.graph[key]).toEqual({
        items: [],
        total_count: {
          status: "unavailable",
          reason: "symbol-tier ingest is deferred in khive.repo.v1",
        },
        bound: { kind: "all", max_items: 0, order: "symbol_id" },
        next_cursor: null,
        truncated: false,
        disclosure: {
          status: "unavailable",
          reason: "symbol-tier ingest is deferred in khive.repo.v1",
        },
      });
    }
  });

  it("requires nested L2 provenance to match the bundle HEAD", () => {
    const value = l2Bundle();
    const codeIngest = value.meta.ingest.code_ingest;
    if (codeIngest.status !== "available") throw new Error("L2 fixture has code provenance");
    const l2 = codeIngest.value.l2 as { source_revision: string };
    l2.source_revision = "0".repeat(40);

    const result = repoBundleSchema.safeParse(value);

    if (result.success) throw new Error("a mismatched L2 revision was accepted");
    expect(result.error.issues).toEqual(expect.arrayContaining([
      expect.objectContaining({
        path: ["meta", "ingest", "code_ingest", "value", "l2", "source_revision"],
        message: "code.ingest L2 revision must equal the bundle HEAD",
      }),
    ]));
  });

  it("requires page keys and symbol kinds to agree", () => {
    const result = repoBundleSchema.safeParse(bundleWithFunctionRow({
      ...completeFunctionRow,
      kind: "datatype",
    }));

    if (result.success) throw new Error("a symbol in the wrong page was accepted");
    expect(result.error.issues).toEqual(expect.arrayContaining([
      expect.objectContaining({
        path: ["graph", "functions", "items", 0, "kind"],
        message: "symbol kind must agree with its page",
      }),
    ]));
  });

  it("requires exact populated truncation disclosure", () => {
    const value = l2Bundle(2);
    value.graph.functions = {
      items: [completeFunctionRow],
      total_count: { status: "available", value: 2 },
      bound: { kind: "top_n", max_items: 1, order: "module_path,name,symbol_id" },
      next_cursor: "offset:1",
      truncated: true,
      disclosure: { status: "truncated", reason: "different wording" },
    };

    const result = repoBundleSchema.safeParse(value);

    if (result.success) throw new Error("incorrect truncation disclosure was accepted");
    expect(result.error.issues).toEqual(expect.arrayContaining([
      expect.objectContaining({
        path: ["graph", "functions"],
        message: "truncated symbol page must use the declared bound, cursor, total, and disclosure",
      }),
    ]));
  });

  it("requires the declared symbol row order", () => {
    const value = l2Bundle(2);
    value.graph.functions = availableSymbolPage([
      { ...completeFunctionRow, id: "later", module_path: "crate::z" },
      { ...completeFunctionRow, id: "earlier", module_path: "crate::a" },
    ]);

    const result = repoBundleSchema.safeParse(value);

    if (result.success) throw new Error("out-of-order symbol rows were accepted");
    expect(result.error.issues).toEqual(expect.arrayContaining([
      expect.objectContaining({
        path: ["graph", "functions", "items", 1],
        message: "symbol page items must follow the declared order",
      }),
    ]));
  });

  it.each([
    ["outgoing_call_edge_count", -1],
    ["outgoing_type_reference_edge_count", 1.5],
    ["incoming_implements_edge_count", -1],
  ])("rejects invalid symbol fact %s", (field, value) => {
    const result = repoBundleSchema.safeParse(bundleWithFunctionRow({
      ...completeFunctionRow,
      [field]: value,
    }));

    if (result.success) throw new Error("an invalid symbol fact was accepted");
    expect(result.error.issues).toEqual(expect.arrayContaining([
      expect.objectContaining({
        path: ["graph", "functions", "items", 0, field],
      }),
    ]));
  });

  it("rejects unknown symbol facts", () => {
    const result = repoBundleSchema.safeParse(bundleWithFunctionRow({
      ...completeFunctionRow,
      guessed_call_edge_count: 4,
    }));

    if (result.success) throw new Error("an unknown symbol fact was accepted");
    expect(result.error.issues).toEqual(expect.arrayContaining([
      expect.objectContaining({
        code: "unrecognized_keys",
        path: ["graph", "functions", "items", 0],
        keys: ["guessed_call_edge_count"],
      }),
    ]));
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
