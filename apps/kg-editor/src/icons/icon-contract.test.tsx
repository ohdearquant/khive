import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { Icon, ICON_NAMES } from "./index";

const SOURCE_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const APP_ROOT = join(SOURCE_ROOT, "..");

function filesBelow(directory: string, suffix: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return filesBelow(path, suffix);
    return entry.name.endsWith(suffix) ? [path] : [];
  });
}

function isDataVisualization(relativePath: string, svgTag: string): boolean {
  if (relativePath === "components/studio.tsx") {
    return svgTag.includes('className="graph-lines"');
  }
  if (relativePath === "components/showcase/repo-showcase.tsx") {
    return svgTag.includes('className="repo-edges"') ||
      svgTag.includes(' data-visualization="hotspot"') ||
      svgTag.includes(' data-visualization="cadence"');
  }
  if (relativePath === "components/ontology-mark.tsx") {
    return svgTag.includes('className="ontology-derived-glyph-icon"');
  }
  return false;
}

afterEach(cleanup);

describe("shared icon contract", () => {
  it("does not exempt an ordinary accessible SVG from icon lint", () => {
    const showcase = "components/showcase/repo-showcase.tsx";
    expect(isDataVisualization(showcase, '<svg role="img">')).toBe(false);
    expect(
      isDataVisualization(
        showcase,
        '<svg role="img" data-visualization="other">',
      ),
    ).toBe(false);
    expect(
      isDataVisualization(
        showcase,
        '<svg role="img" data-visualization="hotspot">',
      ),
    ).toBe(true);
    expect(
      isDataVisualization(
        showcase,
        '<svg role="img" data-visualization="cadence">',
      ),
    ).toBe(true);
    const ontology = "components/ontology-mark.tsx";
    expect(isDataVisualization(ontology, '<svg role="img">')).toBe(false);
    expect(
      isDataVisualization(
        ontology,
        '<svg aria-hidden="true" className="ontology-derived-glyph-icon" viewBox="0 0 24 24">',
      ),
    ).toBe(true);
  });

  it("renders every icon through the same literal SVG contract", () => {
    expect(ICON_NAMES.length).toBeGreaterThan(0);

    for (const name of ICON_NAMES) {
      const { container, unmount } = render(
        <Icon aria-label={name} name={name} />,
      );
      const svg = container.querySelector("svg");
      expect(svg, name).not.toBeNull();
      expect(svg?.getAttribute("width"), name).toBe("24");
      expect(svg?.getAttribute("height"), name).toBe("24");
      expect(svg?.getAttribute("viewBox"), name).toBe("0 0 24 24");
      expect(svg?.getAttribute("fill"), name).toBe("none");
      expect(svg?.getAttribute("stroke"), name).toBe("currentColor");
      expect(svg?.getAttribute("stroke-width"), name).toBe("1.5");
      expect(svg?.getAttribute("stroke-linecap"), name).toBe("round");
      expect(svg?.getAttribute("stroke-linejoin"), name).toBe("round");
      expect(
        svg?.querySelectorAll(
          "path, circle, line, polyline, polygon, rect, ellipse",
        ).length,
        name,
      ).toBeGreaterThan(0);
      expect(svg?.querySelector("text, use, foreignObject"), name).toBeNull();
      unmount();
    }
  });

  it("rejects direct icon-library imports and ad-hoc icon SVGs", () => {
    const findings: string[] = [];

    for (const path of filesBelow(SOURCE_ROOT, ".tsx")) {
      const relativePath = relative(SOURCE_ROOT, path);
      if (relativePath.endsWith(".test.tsx")) continue;
      const source = readFileSync(path, "utf8");
      if (source.includes("lucide-react")) {
        findings.push(`${relativePath}: direct lucide-react import`);
      }
      if (/\p{Extended_Pictographic}/u.test(source)) {
        findings.push(`${relativePath}: emoji icon`);
      }
      if (relativePath === "icons/index.tsx") continue;

      for (const match of source.matchAll(/<svg\b[^>]*>/g)) {
        if (!isDataVisualization(relativePath, match[0])) {
          findings.push(`${relativePath}: ad-hoc SVG`);
        }
      }
    }

    const packageJson = JSON.parse(
      readFileSync(join(APP_ROOT, "package.json"), "utf8"),
    ) as { dependencies?: Record<string, string> };
    for (
      const dependency of [
        "lucide-react",
        "react-icons",
        "@fortawesome/react-fontawesome",
        "@heroicons/react",
      ]
    ) {
      if (packageJson.dependencies?.[dependency]) {
        findings.push(`package.json: alternate icon dependency ${dependency}`);
      }
    }

    expect(findings).toEqual([]);
  });
});
