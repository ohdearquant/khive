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
    sourceFileCount: 1,
    recentActivity: 9,
  },
  {
    moduleId: "module-beta-pack",
    packageId: "package-beta",
    packageLabel: "beta",
    modulePath: "pack",
    sourcePath: "crates/beta/src/pack.rs",
    sourceFileCount: 1,
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
  it("nests modules by package and directory with activity-proportional area", () => {
    const layout = buildStructureTreemap(input);

    expect(layout.areaMetric).toBe("recent_activity");
    expect(layout.packages.map((entry) => entry.label)).toEqual([
      "alpha",
      "beta",
    ]);
    expect(layout.packages[0]?.directories.map((entry) => entry.label)).toEqual([
      "src",
      "tests",
    ]);
    expect(globalModuleArea(layout, "module-alpha-pack")).toBeCloseTo(9 / 14, 8);
    expect(globalModuleArea(layout, "module-beta-pack")).toBeCloseTo(3 / 14, 8);
    expect(globalModuleArea(layout, "module-alpha-integration")).toBeCloseTo(1 / 14, 8);
    expect(globalModuleArea(layout, "module-alpha-smoke")).toBeCloseTo(1 / 14, 8);
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
