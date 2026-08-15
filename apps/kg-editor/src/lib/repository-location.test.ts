import { describe, expect, it } from "vitest";

import type { ViewId } from "@/lib/repo-bundle";
import {
  DEFAULT_STRUCTURE_GRAPH_LOCATION,
  parseRepositoryLocation,
  REPOSITORY_VIEW_IDS,
  type RepositoryLocation,
  repositoryLocationUrl,
} from "@/lib/repository-location";

const repository = "https://github.com/ohdearquant/khive";
const snapshotSha = "0123456789abcdef0123456789abcdef01234567";

describe("repository investigation location", () => {
  it.each(REPOSITORY_VIEW_IDS)(
    "round-trips a shareable %s investigation",
    (view: ViewId) => {
      const location: RepositoryLocation = {
        repository,
        snapshotSha,
        modulePath: "crates/space & signals/src/lib.rs",
        view,
        structureGraph: DEFAULT_STRUCTURE_GRAPH_LOCATION,
      };

      const url = repositoryLocationUrl(
        new URL("https://example.test/?utm_source=demo#analysis"),
        location,
      );
      const parsed = parseRepositoryLocation(url);

      expect(parsed.issues).toEqual([]);
      expect(parsed.location).toEqual(location);
      expect(url.searchParams.get("utm_source")).toBe("demo");
      expect(url.hash).toBe("#analysis");
    },
  );

  it("round-trips a canonical package, lens, and focused coupling pair", () => {
    const left = "crates/khive-db/src/stores/graph.rs";
    const right = "crates/khive-db/src/stores/graph_tests.rs";
    const url = repositoryLocationUrl(new URL("https://example.test/"), {
      repository,
      snapshotSha,
      modulePath: right,
      view: "structure_graph",
      structureGraph: {
        packageName: "khive-db",
        lens: "hidden_coupling",
        couplingPair: [right, left],
      },
    });

    expect(url.searchParams.get("pkg")).toBe("khive-db");
    expect(url.searchParams.get("lens")).toBe("hidden_coupling");
    expect(url.searchParams.getAll("pair")).toEqual([left, right]);
    expect(parseRepositoryLocation(url)).toEqual({
      issues: [],
      location: {
        repository,
        snapshotSha,
        modulePath: right,
        view: "structure_graph",
        structureGraph: {
          packageName: "khive-db",
          lens: "hidden_coupling",
          couplingPair: [left, right],
        },
      },
    });
  });

  it("omits default graph state and strips graph state from other views", () => {
    const defaultUrl = repositoryLocationUrl(new URL("https://example.test/"), {
      repository,
      snapshotSha,
      modulePath: null,
      view: "structure_graph",
      structureGraph: DEFAULT_STRUCTURE_GRAPH_LOCATION,
    });
    expect(defaultUrl.searchParams.has("pkg")).toBe(false);
    expect(defaultUrl.searchParams.has("lens")).toBe(false);
    expect(defaultUrl.searchParams.has("pair")).toBe(false);

    const otherView = repositoryLocationUrl(new URL("https://example.test/"), {
      repository,
      snapshotSha,
      modulePath: null,
      view: "scorecard",
      structureGraph: {
        packageName: "khive-db",
        lens: "hidden_coupling",
        couplingPair: ["crates/a.rs", "crates/b.rs"],
      },
    });
    expect(otherView.searchParams.has("pkg")).toBe(false);
    expect(otherView.searchParams.has("lens")).toBe(false);
    expect(otherView.searchParams.has("pair")).toBe(false);
  });

  it.each([
    ["one pair endpoint", "view=structure_graph&lens=hidden_coupling&pair=crates%2Fa.rs", "pair"],
    [
      "three pair endpoints",
      "view=structure_graph&lens=hidden_coupling&pair=crates%2Fa.rs&pair=crates%2Fb.rs&pair=crates%2Fc.rs",
      "pair",
    ],
    [
      "duplicate pair endpoints",
      "view=structure_graph&lens=hidden_coupling&pair=crates%2Fa.rs&pair=crates%2Fa.rs",
      "pair",
    ],
    [
      "pair under the structure lens",
      "view=structure_graph&pair=crates%2Fa.rs&pair=crates%2Fb.rs",
      "pair",
    ],
    ["unknown lens", "view=structure_graph&lens=everything", "lens"],
    ["duplicate package", "view=structure_graph&pkg=one&pkg=two", "pkg"],
    [
      "graph state on another view",
      "view=scorecard&pkg=khive-db&lens=hidden_coupling&pair=crates%2Fa.rs&pair=crates%2Fb.rs",
      "pkg",
    ],
  ])("rejects %s without accepting ambiguous graph state", (_name, search, parameter) => {
    const parsed = parseRepositoryLocation(
      new URL(`https://example.test/?${search}`),
    );

    expect(parsed.issues).toContainEqual(expect.objectContaining({ parameter }));
    if (parameter === "lens") {
      expect(parsed.location.structureGraph.lens).toBe("structure");
    } else if (parameter === "pkg") {
      expect(parsed.location.structureGraph.packageName).toBeNull();
    } else {
      expect(parsed.location.structureGraph.couplingPair).toBeNull();
    }
  });

  it("invalidates a focused pair when its package scope is ambiguous", () => {
    const parsed = parseRepositoryLocation(new URL(
      "https://example.test/?view=structure_graph&pkg=one&pkg=two&lens=hidden_coupling&pair=crates%2Fa.rs&pair=crates%2Fb.rs",
    ));

    expect(parsed.location.structureGraph.packageName).toBeNull();
    expect(parsed.location.structureGraph.couplingPair).toBeNull();
    expect(parsed.issues).toEqual(expect.arrayContaining([
      expect.objectContaining({ parameter: "pkg" }),
      expect.objectContaining({ parameter: "pair" }),
    ]));
  });

  it.each([
    ["malformed", "at=abc123"],
    ["ambiguous", `at=${snapshotSha}&at=${snapshotSha}`],
  ])(
    "invalidates a focused pair when the snapshot is %s",
    (_name, snapshotSearch) => {
      const parsed = parseRepositoryLocation(new URL(
        `https://example.test/?view=structure_graph&${snapshotSearch}&lens=hidden_coupling&pair=crates%2Fa.rs&pair=crates%2Fb.rs`,
      ));

      expect(parsed.location.snapshotSha).toBeNull();
      expect(parsed.location.structureGraph.couplingPair).toBeNull();
      expect(parsed.issues).toEqual(expect.arrayContaining([
        expect.objectContaining({ parameter: "at" }),
        expect.objectContaining({ parameter: "pair" }),
      ]));
    },
  );

  it.each([
    [
      "duplicate repository",
      `repo=${encodeURIComponent(repository)}&repo=${
        encodeURIComponent(repository)
      }`,
      "repo",
    ],
    ["duplicate module", "module=crates%2Fa.rs&module=crates%2Fb.rs", "module"],
    ["malformed snapshot", "at=abc123", "at"],
    ["unknown view", "view=everything", "view"],
    ["absolute module path", "module=%2Fetc%2Fpasswd", "module"],
    ["parent traversal", "module=crates%2F..%2Fsecret.rs", "module"],
    ["empty path segment", "module=crates%2F%2Fsecret.rs", "module"],
    ["empty module", "module=", "module"],
    ["overlong module", `module=${"a".repeat(1025)}`, "module"],
  ])(
    "rejects %s without accepting the ambiguous value",
    (_name, search, parameter) => {
      const parsed = parseRepositoryLocation(
        new URL(`https://example.test/?${search}`),
      );

      expect(parsed.issues).toContainEqual(
        expect.objectContaining({ parameter }),
      );
      const property = parameter === "repo"
        ? "repository"
        : parameter === "at"
        ? "snapshotSha"
        : parameter === "module"
        ? "modulePath"
        : "view";
      expect(parsed.location[property]).toBeNull();
    },
  );

  it("canonicalizes only the closed location parameters in stable order", () => {
    const url = repositoryLocationUrl(
      new URL(
        "https://example.test/?view=scorecard&module=old.rs&at=old&repo=old&keep=1",
      ),
      {
        repository,
        snapshotSha,
        modulePath: "crates/khive-db/src/pool.rs",
        view: "dependency_topology",
        structureGraph: DEFAULT_STRUCTURE_GRAPH_LOCATION,
      },
    );

    expect(url.search).toBe(
      `?keep=1&repo=${
        encodeURIComponent(repository)
      }&at=${snapshotSha}&module=crates%2Fkhive-db%2Fsrc%2Fpool.rs&view=dependency_topology`,
    );
  });
});
