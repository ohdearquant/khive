import { describe, expect, it } from "vitest";

import {
  buildStructureTreemap,
  type StructureTreemapInput,
} from "@/lib/structure-treemap";

const input: StructureTreemapInput[] = [
  {
    moduleId: "module-alpha-pack",
    packageId: "package-alpha",
    packageLabel: "alpha",
    modulePath: "pack",
    sourcePath: "crates/alpha/src/pack.rs",
    sourceFileCount: 4,
    recentActivity: 9,
  },
  {
    moduleId: "module-beta-pack",
    packageId: "package-beta",
    packageLabel: "beta",
    modulePath: "pack",
    sourcePath: "crates/beta/src/pack.rs",
    sourceFileCount: 2,
    recentActivity: 3,
  },
  {
    moduleId: "module-alpha-integration",
    packageId: "package-alpha",
    packageLabel: "alpha",
    modulePath: "tests::integration",
    sourcePath: "crates/alpha/tests/integration.rs",
    sourceFileCount: 1,
    recentActivity: 1,
  },
  {
    moduleId: "module-alpha-smoke",
    packageId: "package-alpha",
    packageLabel: "alpha",
    modulePath: "tests::smoke",
    sourcePath: "crates/alpha/tests/smoke.rs",
    sourceFileCount: 1,
    recentActivity: 1,
  },
];

function area(rect: { width: number; height: number }): number {
  return rect.width * rect.height;
}

function globalModuleArea(
  layout: ReturnType<typeof buildStructureTreemap>,
  moduleId: string,
): number {
  for (const packageLayout of layout.packages) {
    for (const directory of packageLayout.directories) {
      const moduleLayout = directory.modules.find((module) =>
        module.moduleId === moduleId
      );
      if (moduleLayout) {
        return area(packageLayout.rect) * area(directory.rect) *
          area(moduleLayout.rect);
      }
    }
  }
  throw new Error(`missing module layout ${moduleId}`);
}

describe("structure treemap layout", () => {
  it("nests modules by package and directory sized by source-file count", () => {
    const layout = buildStructureTreemap(input);

    expect(layout.areaMetric).toBe("source_file_count");
    expect(layout.activityColoring).toBe("full");
    expect(layout.packages.map((entry) => entry.label)).toEqual([
      "alpha",
      "beta",
    ]);
    expect(layout.packages[0]?.directories.map((entry) => entry.label)).toEqual([
      "src",
      "tests",
    ]);
    // Package share x directory share x module share; each level fills its
    // parent's body rectangle (label clearance is a renderer/pixel concern,
    // not part of the normalized layout).
    expect(globalModuleArea(layout, "module-alpha-pack"))
      .toBeCloseTo((6 / 8) * (4 / 6), 8);
    expect(globalModuleArea(layout, "module-beta-pack"))
      .toBeCloseTo(2 / 8, 8);
    expect(globalModuleArea(layout, "module-alpha-integration"))
      .toBeCloseTo((6 / 8) * (2 / 6) * 0.5, 8);
    expect(globalModuleArea(layout, "module-alpha-smoke"))
      .toBeCloseTo((6 / 8) * (2 / 6) * 0.5, 8);
  });

  it("normalizes activity into color intensity and never into area", () => {
    const layout = buildStructureTreemap(input);
    const byId = new Map(
      layout.packages.flatMap((packageLayout) =>
        packageLayout.directories.flatMap((directory) =>
          directory.modules.map((module) => [module.moduleId, module] as const)
        )
      ),
    );

    expect(byId.get("module-alpha-pack")?.activityIntensity).toBeCloseTo(1, 8);
    expect(byId.get("module-beta-pack")?.activityIntensity).toBeCloseTo(3 / 9, 8);
    expect(byId.get("module-alpha-integration")?.activityIntensity)
      .toBeCloseTo(1 / 9, 8);
  });

  it("keeps zero-file and activity-unavailable modules visible without inventing area", () => {
    const rows: StructureTreemapInput[] = [
      {
        moduleId: "module-zero",
        packageId: "package-solo",
        packageLabel: "solo",
        modulePath: "zero",
        sourcePath: "crates/solo/src/zero.rs",
        sourceFileCount: 0,
        recentActivity: null,
      },
      {
        moduleId: "module-one",
        packageId: "package-solo",
        packageLabel: "solo",
        modulePath: "one",
        sourcePath: "crates/solo/src/one.rs",
        sourceFileCount: 3,
        recentActivity: 5,
      },
    ];
    const layout = buildStructureTreemap(rows);

    expect(layout.activityColoring).toBe("partial");
    const modules = layout.packages[0]!.directories[0]!.modules;
    const zero = modules.find((module) => module.moduleId === "module-zero")!;
    const one = modules.find((module) => module.moduleId === "module-one")!;
    // Deliberate minimum-area policy: a zero-file module keeps one weight
    // unit as a clickable hit target.
    expect(zero.weight).toBe(1);
    expect(one.weight).toBe(3);
    expect(zero.activityIntensity).toBeNull();
    expect(one.activityIntensity).toBeCloseTo(1, 8);
  });

  it("adds package and directory context to duplicate leaf labels", () => {
    const layout = buildStructureTreemap(input);
    const packs = layout.packages.flatMap((packageLayout) =>
      packageLayout.directories.flatMap((directory) =>
        directory.modules.filter((module) => module.leafLabel === "pack")
      )
    );

    expect(packs).toHaveLength(2);
    expect(packs.map((module) => module.parentLabel)).toEqual([
      "alpha · src",
      "beta · src",
    ]);
  });

  it("is deterministic when source rows arrive in a different order", () => {
    expect(buildStructureTreemap([...input].reverse())).toEqual(
      buildStructureTreemap(input),
    );
  });
});
