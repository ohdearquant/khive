import { mkdir, mkdtemp, readFile, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import {
  loadMaterializedShowcaseAnalysis,
  resolveShowcaseAnalysisRegistry,
  ShowcaseAnalysisError,
} from "@/lib/server/materialized-showcase-source";

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);

const temporaryRoots: string[] = [];

async function temporaryRoot(): Promise<string> {
  const root = await mkdtemp(resolve(tmpdir(), "khive-showcase-analysis-"));
  temporaryRoots.push(root);
  return root;
}

async function writeAnalysis(
  root: string,
  id: string,
  bytes: Uint8Array,
): Promise<string> {
  const path = resolve(root, id, "khive.repo.v1.json");
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, bytes);
  return path;
}

afterEach(async () => {
  const { rm } = await import("node:fs/promises");
  await Promise.all(
    temporaryRoots.splice(0).map((root) => rm(root, { recursive: true })),
  );
});

describe("materialized showcase analysis source", () => {
  it("resolves only an explicit closed analysis-id registry", () => {
    const root = "/server/private/analyses";
    expect(resolveShowcaseAnalysisRegistry({
      KHIVE_SHOWCASE_ANALYSIS_ROOT: root,
      KHIVE_SHOWCASE_ANALYSIS_IDS: "khive, example-2",
    })).toEqual({ root, ids: new Set(["khive", "example-2"]) });

    expect(() =>
      resolveShowcaseAnalysisRegistry({
        KHIVE_SHOWCASE_ANALYSIS_ROOT: root,
        KHIVE_SHOWCASE_ANALYSIS_IDS: "khive,../escape",
      })
    ).toThrow(ShowcaseAnalysisError);
  });

  it("loads and validates a server-private DB-produced snapshot", async () => {
    const root = await temporaryRoot();
    const bytes = await readFile(goldenPath);
    await writeAnalysis(root, "khive", bytes);

    const result = await loadMaterializedShowcaseAnalysis(
      "khive",
      { root, ids: new Set(["khive"]) },
    );

    expect(result.bundle.schema_version).toBe("khive.repo.v1");
    expect(result.bundle.meta.snapshot.head_sha).toMatch(/^[0-9a-f]{40}$/);
    expect(Buffer.from(result.bytes).equals(bytes)).toBe(true);
    expect(result.etag).toMatch(/^"sha256-[0-9a-f]{64}"$/);
  }, 20_000);

  it("rejects unknown and traversal-shaped ids before filesystem access", async () => {
    const root = await temporaryRoot();
    for (const id of ["unknown", "../khive", "khive/../../escape"]) {
      await expect(loadMaterializedShowcaseAnalysis(
        id,
        { root, ids: new Set(["khive"]) },
      )).rejects.toMatchObject({ code: "NOT_CONFIGURED" });
    }
  });

  it.runIf(process.platform !== "win32")(
    "refuses a symlinked report even when it resolves under the configured root",
    async () => {
      const root = await temporaryRoot();
      const bytes = await readFile(goldenPath);
      const target = await writeAnalysis(root, "source", bytes);
      const linked = resolve(root, "khive", "khive.repo.v1.json");
      await mkdir(dirname(linked), { recursive: true });
      await symlink(target, linked);

      await expect(loadMaterializedShowcaseAnalysis(
        "khive",
        { root, ids: new Set(["khive"]) },
      )).rejects.toMatchObject({ code: "ANALYSIS_INVALID" });
    },
  );

  it.runIf(process.platform !== "win32")(
    "refuses a symlinked analysis directory",
    async () => {
      const root = await temporaryRoot();
      const bytes = await readFile(goldenPath);
      await writeAnalysis(root, "source", bytes);
      await symlink(resolve(root, "source"), resolve(root, "khive"), "dir");

      await expect(loadMaterializedShowcaseAnalysis(
        "khive",
        { root, ids: new Set(["khive"]) },
      )).rejects.toMatchObject({ code: "ANALYSIS_INVALID" });
    },
  );

  it("fails closed on oversized or invalid reports without exposing their paths", async () => {
    const root = await temporaryRoot();
    await writeAnalysis(root, "large", new Uint8Array((8 * 1024 * 1024) + 1));
    await writeAnalysis(root, "invalid", new TextEncoder().encode("not json"));

    await expect(loadMaterializedShowcaseAnalysis(
      "large",
      { root, ids: new Set(["large"]) },
    )).rejects.toMatchObject({ code: "ANALYSIS_TOO_LARGE" });

    let error: unknown;
    try {
      await loadMaterializedShowcaseAnalysis(
        "invalid",
        { root, ids: new Set(["invalid"]) },
      );
    } catch (caught) {
      error = caught;
    }
    expect(error).toMatchObject({ code: "ANALYSIS_INVALID" });
    expect(String(error)).not.toContain(root);
  });
});
