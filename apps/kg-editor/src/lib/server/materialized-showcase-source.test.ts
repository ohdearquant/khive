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
  it("resolves a normalized, deterministic explicit analysis registry", () => {
    const root = "/server/private/analyses";
    expect(resolveShowcaseAnalysisRegistry({
      KHIVE_SHOWCASE_ANALYSIS_ROOT: root,
      KHIVE_SHOWCASE_ANALYSES: JSON.stringify([
        {
          analysis_id: "khive",
          canonical_url:
            "http://www.github.com/ohdearquant/khive.git?tab=readme",
        },
        {
          analysis_id: "example-2",
          canonical_url: "https://github.com/example/two/",
        },
      ]),
    })).toEqual({
      root,
      entries: [
        {
          analysis_id: "example-2",
          canonical_url: "https://github.com/example/two",
        },
        {
          analysis_id: "khive",
          canonical_url: "https://github.com/ohdearquant/khive",
        },
      ],
    });
  });

  it("fails closed on malformed, non-strict, empty, or oversized registries", () => {
    const root = "/server/private/analyses";
    const entries = Array.from({ length: 65 }, (_, index) => ({
      analysis_id: `repo-${index}`,
      canonical_url: `https://github.com/example/repo-${index}`,
    }));

    for (
      const configured of [
        "not json",
        "{}",
        "[]",
        JSON.stringify([{
          analysis_id: "khive",
          canonical_url: "https://github.com/ohdearquant/khive",
          path: "/private/report.json",
        }]),
        JSON.stringify([{
          analysis_id: "../khive",
          canonical_url: "https://github.com/ohdearquant/khive",
        }]),
        JSON.stringify([{
          analysis_id: "khive",
          canonical_url: "file:///server/private/khive",
        }]),
        JSON.stringify(entries),
      ]
    ) {
      expect(() =>
        resolveShowcaseAnalysisRegistry({
          KHIVE_SHOWCASE_ANALYSIS_ROOT: root,
          KHIVE_SHOWCASE_ANALYSES: configured,
        })
      ).toThrow(ShowcaseAnalysisError);
    }
  });

  it("fails closed when ids or normalized repository URLs collide", () => {
    const root = "/server/private/analyses";
    const configurations = [
      [
        {
          analysis_id: "khive",
          canonical_url: "https://github.com/ohdearquant/khive",
        },
        {
          analysis_id: "khive",
          canonical_url: "https://github.com/example/other",
        },
      ],
      [
        {
          analysis_id: "khive",
          canonical_url: "https://github.com/ohdearquant/khive",
        },
        {
          analysis_id: "khive-alias",
          canonical_url: "http://www.github.com/ohdearquant/khive.git",
        },
      ],
    ];

    for (const entries of configurations) {
      expect(() =>
        resolveShowcaseAnalysisRegistry({
          KHIVE_SHOWCASE_ANALYSIS_ROOT: root,
          KHIVE_SHOWCASE_ANALYSES: JSON.stringify(entries),
        })
      ).toThrow(ShowcaseAnalysisError);
    }
  });

  it("does not accept the removed id-only allowlist", () => {
    expect(() =>
      resolveShowcaseAnalysisRegistry({
        KHIVE_SHOWCASE_ANALYSIS_ROOT: "/server/private/analyses",
        KHIVE_SHOWCASE_ANALYSIS_IDS: "khive",
      })
    ).toThrow(ShowcaseAnalysisError);
  });

  it("requires an absolute operator-owned analysis root", () => {
    for (const root of [undefined, "", "relative/analyses"]) {
      expect(() =>
        resolveShowcaseAnalysisRegistry({
          KHIVE_SHOWCASE_ANALYSIS_ROOT: root,
          KHIVE_SHOWCASE_ANALYSES: JSON.stringify([{
            analysis_id: "khive",
            canonical_url: "https://github.com/ohdearquant/khive",
          }]),
        })
      ).toThrow(ShowcaseAnalysisError);
    }
  });

  it("loads and validates a server-private DB-produced snapshot", async () => {
    const root = await temporaryRoot();
    const bytes = await readFile(goldenPath);
    await writeAnalysis(root, "khive", bytes);

    const result = await loadMaterializedShowcaseAnalysis(
      "khive",
      {
        root,
        entries: [{
          analysis_id: "khive",
          canonical_url: "https://github.com/ohdearquant/khive",
        }],
      },
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
        {
          root,
          entries: [{
            analysis_id: "khive",
            canonical_url: "https://github.com/ohdearquant/khive",
          }],
        },
      )).rejects.toMatchObject({ code: "NOT_CONFIGURED" });
    }
  });

  it(
    "rejects a valid bundle whose repository URL is not configured for its id",
    async () => {
      const root = await temporaryRoot();
      await writeAnalysis(root, "khive", await readFile(goldenPath));

      await expect(loadMaterializedShowcaseAnalysis(
        "khive",
        {
          root,
          entries: [{
            analysis_id: "khive",
            canonical_url: "https://github.com/example/different-repository",
          }],
        },
      )).rejects.toMatchObject({ code: "ANALYSIS_INVALID" });
    },
    20_000,
  );

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
        {
          root,
          entries: [{
            analysis_id: "khive",
            canonical_url: "https://github.com/ohdearquant/khive",
          }],
        },
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
        {
          root,
          entries: [{
            analysis_id: "khive",
            canonical_url: "https://github.com/ohdearquant/khive",
          }],
        },
      )).rejects.toMatchObject({ code: "ANALYSIS_INVALID" });
    },
  );

  it("fails closed on oversized or invalid reports without exposing their paths", async () => {
    const root = await temporaryRoot();
    await writeAnalysis(root, "large", new Uint8Array((8 * 1024 * 1024) + 1));
    await writeAnalysis(root, "invalid", new TextEncoder().encode("not json"));

    await expect(loadMaterializedShowcaseAnalysis(
      "large",
      {
        root,
        entries: [{
          analysis_id: "large",
          canonical_url: "https://github.com/example/large",
        }],
      },
    )).rejects.toMatchObject({ code: "ANALYSIS_TOO_LARGE" });

    let error: unknown;
    try {
      await loadMaterializedShowcaseAnalysis(
        "invalid",
        {
          root,
          entries: [{
            analysis_id: "invalid",
            canonical_url: "https://github.com/example/invalid",
          }],
        },
      );
    } catch (caught) {
      error = caught;
    }
    expect(error).toMatchObject({ code: "ANALYSIS_INVALID" });
    expect(String(error)).not.toContain(root);
  });
});
