export type TreemapRect = Readonly<{
  x: number;
  y: number;
  width: number;
  height: number;
}>;

export type StructureTreemapInput = Readonly<{
  moduleId: string;
  packageId: string;
  packageLabel: string;
  modulePath: string;
  sourcePath: string;
  sourceFileCount: number;
  recentActivity: number | null;
}>;

export type StructureTreemapModule = Readonly<{
  moduleId: string;
  sourcePath: string;
  leafLabel: string;
  parentLabel: string;
  recentActivity: number | null;
  sourceFileCount: number;
  weight: number;
  rect: TreemapRect;
}>;

export type StructureTreemapDirectory = Readonly<{
  id: string;
  label: string;
  weight: number;
  rect: TreemapRect;
  modules: readonly StructureTreemapModule[];
}>;

export type StructureTreemapPackage = Readonly<{
  id: string;
  label: string;
  tone: number;
  weight: number;
  rect: TreemapRect;
  directories: readonly StructureTreemapDirectory[];
}>;

export type StructureTreemapLayout = Readonly<{
  areaMetric: "recent_activity" | "recent_activity_with_source_file_fallback";
  packages: readonly StructureTreemapPackage[];
}>;

type Weighted<T> = Readonly<{
  key: string;
  weight: number;
  value: T;
}>;

const UNIT_RECT: TreemapRect = { x: 0, y: 0, width: 1, height: 1 };
const PACKAGE_TONE_COUNT = 9;

function compareText(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function sumWeight<T>(entries: readonly Weighted<T>[]): number {
  return entries.reduce((total, entry) => total + entry.weight, 0);
}

function layoutWeighted<T>(
  source: readonly Weighted<T>[],
  rect: TreemapRect = UNIT_RECT,
): Array<Weighted<T> & { rect: TreemapRect }> {
  if (source.length === 0) return [];
  if (source.length === 1) return [{ ...source[0], rect }];

  const total = sumWeight(source);
  let prefix = 0;
  let splitIndex = 1;
  let bestDistance = Number.POSITIVE_INFINITY;
  for (let index = 1; index < source.length; index += 1) {
    prefix += source[index - 1]!.weight;
    const distance = Math.abs(total / 2 - prefix);
    if (distance < bestDistance) {
      bestDistance = distance;
      splitIndex = index;
    }
  }

  const left = source.slice(0, splitIndex);
  const right = source.slice(splitIndex);
  const leftRatio = sumWeight(left) / total;
  const splitHorizontally = rect.width >= rect.height;
  const leftRect: TreemapRect = splitHorizontally
    ? { ...rect, width: rect.width * leftRatio }
    : { ...rect, height: rect.height * leftRatio };
  const rightRect: TreemapRect = splitHorizontally
    ? {
      x: rect.x + leftRect.width,
      y: rect.y,
      width: rect.width - leftRect.width,
      height: rect.height,
    }
    : {
      x: rect.x,
      y: rect.y + leftRect.height,
      width: rect.width,
      height: rect.height - leftRect.height,
    };

  return [
    ...layoutWeighted(left, leftRect),
    ...layoutWeighted(right, rightRect),
  ];
}

function directoryLabel(input: StructureTreemapInput): string {
  const segments = input.sourcePath.split("/").filter(Boolean);
  const packageIndex = segments.lastIndexOf(input.packageLabel);
  const directory = segments.slice(
    packageIndex >= 0 ? packageIndex + 1 : 0,
    -1,
  );
  return directory.join("/") || "crate root";
}

function leafLabel(modulePath: string): string {
  const segments = modulePath.split("::").filter(Boolean);
  return segments.at(-1) || modulePath;
}

function moduleWeight(input: StructureTreemapInput): number {
  const measure = input.recentActivity ?? input.sourceFileCount;
  return Math.max(1, Number.isFinite(measure) ? measure : 1);
}

function packageTone(packageId: string): number {
  let hash = 2_166_136_261;
  for (const codePoint of packageId) {
    hash ^= codePoint.codePointAt(0) ?? 0;
    hash = Math.imul(hash, 16_777_619);
  }
  return (hash >>> 0) % PACKAGE_TONE_COUNT;
}

export function buildStructureTreemap(
  input: readonly StructureTreemapInput[],
): StructureTreemapLayout {
  const packageInputs = new Map<string, StructureTreemapInput[]>();
  for (const row of input) {
    const rows = packageInputs.get(row.packageId) ?? [];
    rows.push(row);
    packageInputs.set(row.packageId, rows);
  }

  const packages = [...packageInputs.entries()]
    .map(([packageId, rows]) => ({
      id: packageId,
      label: rows[0]?.packageLabel ?? packageId,
      rows,
      weight: rows.reduce((total, row) => total + moduleWeight(row), 0),
    }))
    .sort((left, right) =>
      compareText(left.label, right.label) || compareText(left.id, right.id)
    );

  const packageRects = layoutWeighted(packages.map((entry) => ({
    key: entry.id,
    weight: entry.weight,
    value: entry,
  })));

  return {
    areaMetric: input.every((row) => row.recentActivity !== null)
      ? "recent_activity"
      : "recent_activity_with_source_file_fallback",
    packages: packageRects.map(({ value: packageEntry, rect: packageRect }) => {
      const directoryInputs = new Map<string, StructureTreemapInput[]>();
      for (const row of packageEntry.rows) {
        const label = directoryLabel(row);
        const rows = directoryInputs.get(label) ?? [];
        rows.push(row);
        directoryInputs.set(label, rows);
      }
      const directories = [...directoryInputs.entries()]
        .map(([label, rows]) => ({
          id: `${packageEntry.id}:${label}`,
          label,
          rows,
          weight: rows.reduce((total, row) => total + moduleWeight(row), 0),
        }))
        .sort((left, right) => compareText(left.label, right.label));
      const directoryRects = layoutWeighted(directories.map((entry) => ({
        key: entry.id,
        weight: entry.weight,
        value: entry,
      })));

      return {
        id: packageEntry.id,
        label: packageEntry.label,
        tone: packageTone(packageEntry.id),
        weight: packageEntry.weight,
        rect: packageRect,
        directories: directoryRects.map(({
          value: directory,
          rect: directoryRect,
        }) => {
          const modules = [...directory.rows].sort((left, right) =>
            compareText(left.sourcePath, right.sourcePath) ||
            compareText(left.moduleId, right.moduleId)
          );
          const moduleRects = layoutWeighted(modules.map((row) => ({
            key: row.moduleId,
            weight: moduleWeight(row),
            value: row,
          })));
          return {
            id: directory.id,
            label: directory.label,
            weight: directory.weight,
            rect: directoryRect,
            modules: moduleRects.map(({ value: row, weight, rect }) => ({
              moduleId: row.moduleId,
              sourcePath: row.sourcePath,
              leafLabel: leafLabel(row.modulePath),
              parentLabel: `${packageEntry.label} · ${directory.label}`,
              recentActivity: row.recentActivity,
              sourceFileCount: row.sourceFileCount,
              weight,
              rect,
            })),
          };
        }),
      };
    }),
  };
}
