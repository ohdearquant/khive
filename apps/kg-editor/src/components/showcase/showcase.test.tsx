import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  loadPreferredShowcaseBundle,
  readOperatorShowcaseAccessToken,
  ShowcaseAnalysisNotFoundError,
} from "@/lib/adapters/preferred-showcase-source";
import { loadShowcaseAnalysisCatalog } from "@/lib/adapters/showcase-analysis-catalog";
import { parseRepoBundle } from "@/lib/repo-bundle";

vi.mock("@/lib/adapters/preferred-showcase-source", () => ({
  loadPreferredShowcaseBundle: vi.fn(),
  readOperatorShowcaseAccessToken: vi.fn(() => null),
  ShowcaseAnalysisNotFoundError: class ShowcaseAnalysisNotFoundError extends Error {
    canonicalUrl: string;

    constructor(canonicalUrl: string) {
      super("The configured repository analysis is not available.");
      this.canonicalUrl = canonicalUrl;
    }
  },
}));

vi.mock("@/lib/adapters/showcase-analysis-catalog", async (importOriginal) => ({
  ...await importOriginal<typeof import("@/lib/adapters/showcase-analysis-catalog")>(),
  loadShowcaseAnalysisCatalog: vi.fn(),
}));

import { Showcase } from "@/components/showcase/showcase";

const goldenPath = resolve(process.cwd(), "../../docs/schemas/examples/khive-repo-v1-khive.json");
const bundle = parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
const mockedLoad = vi.mocked(loadPreferredShowcaseBundle);
const mockedCatalog = vi.mocked(loadShowcaseAnalysisCatalog);
const mockedAccessToken = vi.mocked(readOperatorShowcaseAccessToken);
const defaultCatalogEntry = {
  analysis_id: "khive",
  canonical_url: "https://github.com/ohdearquant/khive",
} as const;

describe("materialized repository lookup", () => {
  beforeEach(() => {
    window.history.replaceState(null, "", "/");
    mockedLoad.mockReset();
    mockedLoad.mockResolvedValue({ bundle, source: "khive-db-snapshot" });
    mockedCatalog.mockReset();
    mockedCatalog.mockResolvedValue({
      status: "ready",
      entries: [defaultCatalogEntry],
      message: "1 configured repository analysis discovered.",
    });
    mockedAccessToken.mockReset();
    mockedAccessToken.mockReturnValue(null);
  });

  it("resolves the repository in a direct URL before loading the default", async () => {
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent("https://github.com/example/not-curated")}&at=${bundle.meta.snapshot.head_sha}&module=crates%2Fexample%2Fsrc%2Flib.rs&view=scorecard`,
    );

    render(<Showcase />);

    expect(
      await screen.findByText("No curated showcase bundle matches this repository"),
    ).toBeVisible();
    expect(mockedLoad).not.toHaveBeenCalled();
    expect(new URL(window.location.href).searchParams.get("repo")).toBe(
      "https://github.com/example/not-curated",
    );
  });

  it("normalizes a curated alias, reauthorizes each private snapshot load, and performs no bundle load for a later miss", async () => {
    const user = userEvent.setup();
    const { container } = render(<Showcase />);

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    expect(mockedLoad).toHaveBeenCalledTimes(1);
    expect(container.querySelector(".repo-overview")).toHaveAttribute(
      "data-head-sha",
      "c2979d2443738a075e55a170c772d1dc86cf0f91",
    );
    expect(container.querySelector(".repo-overview")).toHaveAttribute(
      "data-analysis-source",
      "khive-db-snapshot",
    );
    expect(screen.getAllByText(/khive DB snapshot/i).length).toBeGreaterThan(0);

    const input = screen.getByLabelText("Public repository URL");
    await user.clear(input);
    await user.type(input, "http://github.com/ohdearquant/khive.git");
    await user.click(screen.getByRole("button", { name: bundle.capability.labels.lookup_action }));

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    expect(window.location.search).toContain(
      "repo=https%3A%2F%2Fgithub.com%2Fohdearquant%2Fkhive",
    );
    // Private snapshots are authorized per load: the same entry loads again
    // rather than being served from the module cache.
    expect(mockedLoad).toHaveBeenCalledTimes(2);

    window.history.replaceState(
      null,
      "",
      `${window.location.pathname}${window.location.search}&at=${bundle.meta.snapshot.head_sha}&module=crates%2Fkhive-db%2Fsrc%2Fpool.rs&view=dependency_topology`,
    );
    await user.clear(input);
    await user.type(input, "https://github.com/example/not-curated");
    await user.click(screen.getByRole("button", { name: bundle.capability.labels.lookup_action }));

    expect(await screen.findByText(bundle.capability.labels.miss_title)).toBeVisible();
    expect(screen.getByText(new RegExp(bundle.capability.labels.miss_body))).toBeVisible();
    const empty = container.querySelector<HTMLElement>('[data-state="empty"]');
    expect(empty).toBeVisible();
    expect(empty?.querySelectorAll("button")).toHaveLength(1);
    expect(window.location.search).toBe("");
    expect(new URL(window.location.href).searchParams.get("at")).toBeNull();
    expect(new URL(window.location.href).searchParams.get("module")).toBeNull();
    expect(new URL(window.location.href).searchParams.get("view")).toBeNull();
    // Private snapshots are authorized per load, so the miss leaves the
    // count at two loads rather than one served from a module cache.
    expect(mockedLoad).toHaveBeenCalledTimes(2);

    await user.click(screen.getByRole("button", { name: "Use the curated khive example" }));
    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    expect(mockedLoad).toHaveBeenCalledTimes(3);
  }, 30_000);

  it("serves the public static fallback from cache instead of reloading it", async () => {
    mockedLoad.mockReset();
    mockedLoad.mockResolvedValue({ bundle, source: "curated-static-fallback" });
    const user = userEvent.setup();
    const { container } = render(<Showcase />);

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    expect(mockedLoad).toHaveBeenCalledTimes(1);
    expect(container.querySelector(".repo-overview")).toHaveAttribute(
      "data-analysis-source",
      "curated-static-fallback",
    );

    const input = screen.getByLabelText("Public repository URL");
    await user.clear(input);
    await user.type(input, "http://github.com/ohdearquant/khive.git");
    await user.click(screen.getByRole("button", { name: bundle.capability.labels.lookup_action }));

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    // Nothing private is retained here, so the cache may keep serving it.
    expect(mockedLoad).toHaveBeenCalledTimes(1);
    // Measured ~2.5-4.9s locally across several userEvent interactions and bundle loads;
    // full-suite CPU contention pushed it past the default 5s timeout, so this needs headroom.
  }, 30_000);

  it.each([
    ["query string", `${bundle.meta.repository.canonical_url}?tab=readme`],
    ["fragment", `${bundle.meta.repository.canonical_url}#readme`],
  ])(
    "opens a direct link whose curated repository alias carries a %s",
    async (_name, repositoryWithExtras) => {
      window.history.replaceState(
        null,
        "",
        `/?repo=${encodeURIComponent(repositoryWithExtras)}`,
      );

      const { container } = render(<Showcase />);

      await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
      expect(
        screen.queryByText("No curated showcase bundle matches this repository"),
      ).not.toBeInTheDocument();
      expect(new URL(window.location.href).searchParams.get("repo")).toBe(
        bundle.meta.repository.canonical_url,
      );
    },
  );

  it("preserves a direct investigation while canonicalizing a curated alias", async () => {
    const pool = bundle.graph.modules.items.find((module) =>
      module.source_path.endsWith("khive-db/src/pool.rs")
    )!;
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent("http://github.com/ohdearquant/khive.git")}&at=${bundle.meta.snapshot.head_sha}&module=${encodeURIComponent(pool.source_path)}&view=dependency_topology`,
    );

    const { container } = render(<Showcase />);

    const inspector = await screen.findByRole("complementary", {
      name: "Module evidence",
    });
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 })).toHaveTextContent(
        pool.source_path,
      )
    );
    expect(container.querySelector('[data-view-id="dependency_topology"]'))
      .toHaveAttribute("aria-current", "page");
    expect(new URL(window.location.href).searchParams.get("repo")).toBe(
      bundle.meta.repository.canonical_url,
    );
    expect(new URL(window.location.href).searchParams.get("module")).toBe(
      pool.source_path,
    );
  });

  it("sends the operator bearer token on catalog discovery and still merges dynamic entries", async () => {
    mockedAccessToken.mockReturnValue("operator-secret");
    const actualCatalog = await vi.importActual<
      typeof import("@/lib/adapters/showcase-analysis-catalog")
    >("@/lib/adapters/showcase-analysis-catalog");
    const dynamicUrl = "https://github.com/example/header-check-only";
    const dynamicAnalysisId = "header-check-only";
    const body = new TextEncoder().encode(JSON.stringify({
      schema_version: "khive.showcase.catalog.v1",
      entries: [{ analysis_id: dynamicAnalysisId, canonical_url: dynamicUrl }],
    }));
    const fetchCatalog = vi.fn(async () => ({
      ok: true,
      status: 200,
      headers: new Headers({ "content-length": String(body.byteLength) }),
      arrayBuffer: async () => body.buffer as ArrayBuffer,
      body: null,
    }));
    mockedCatalog.mockImplementation(() =>
      actualCatalog.loadShowcaseAnalysisCatalog(fetchCatalog)
    );

    render(<Showcase />);

    await waitFor(() =>
      expect(screen.getByRole("option", { name: dynamicUrl })).toBeInTheDocument()
    );
    expect(fetchCatalog).toHaveBeenCalledWith(
      "/api/showcase/analyses",
      expect.objectContaining({
        headers: { authorization: "Bearer operator-secret" },
      }),
    );
  });

  it("waits for catalog discovery before resolving a dynamic deep link", async () => {
    const dynamicAnalysisId = "deep-link-only";
    let releaseCatalog: ((value: Awaited<ReturnType<typeof loadShowcaseAnalysisCatalog>>) => void) | undefined;
    mockedCatalog.mockReturnValue(new Promise((resolveCatalog) => {
      releaseCatalog = resolveCatalog;
    }));
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent("https://github.com/example/dynamic-only")}`,
    );

    render(<Showcase />);

    const selector = screen.getByRole("combobox", { name: "Repository analysis" });
    expect(selector).toHaveAttribute("aria-busy", "true");
    expect(mockedLoad).not.toHaveBeenCalled();

    releaseCatalog?.({
      status: "ready",
      entries: [{
        analysis_id: dynamicAnalysisId,
        canonical_url: "https://github.com/example/dynamic-only",
      }],
      message: "1 configured repository analysis discovered.",
    });

    await waitFor(() => expect(mockedLoad).toHaveBeenCalledWith(
      expect.objectContaining({
        id: `analysis:${dynamicAnalysisId}`,
        analysisId: dynamicAnalysisId,
        assetPath: undefined,
      }),
      expect.any(Function),
      { accessToken: null },
    ));
  });

  it("keeps the curated static repository usable when the catalog degrades", async () => {
    mockedCatalog.mockResolvedValue({
      status: "degraded",
      entries: [],
      message: "The server analysis catalog is unavailable; curated static repositories remain available.",
    });
    mockedLoad.mockResolvedValue({
      bundle,
      source: "curated-static-fallback",
    });

    const { container } = render(<Showcase />);

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    expect(screen.getByRole("status", { name: "Repository catalog status" }))
      .toHaveTextContent(/catalog is unavailable.*static repositories remain available/i);
    expect(screen.getByRole("combobox", { name: "Repository analysis" }))
      .toHaveValue("github.com/ohdearquant/khive");
    expect(screen.getByLabelText("Analysis source")).toHaveTextContent(
      "curated static fallback",
    );
    expect(mockedLoad).toHaveBeenCalledWith(
      expect.objectContaining({
        assetPath: "/showcase/khive-repo-v1-khive.json",
        analysisId: undefined,
      }),
      expect.any(Function),
      { accessToken: null },
    );
  });

  it("switches a native repository selector and exposes URL, source, busy, and status", async () => {
    const user = userEvent.setup();
    const dynamicAnalysisId = "selector-only";
    const dynamicUrl = "https://github.com/example/dynamic-only";
    const dynamicBundle = {
      ...bundle,
      meta: {
        ...bundle.meta,
        repository: {
          ...bundle.meta.repository,
          canonical_url: dynamicUrl,
        },
      },
    };
    let releaseDynamic: (() => void) | undefined;
    mockedCatalog.mockResolvedValue({
      status: "ready",
      entries: [
        {
          analysis_id: dynamicAnalysisId,
          canonical_url: dynamicUrl,
        },
        defaultCatalogEntry,
      ],
      message: "2 configured repository analyses discovered.",
    });
    mockedLoad.mockImplementation((entry) => {
      if (entry.analysisId !== dynamicAnalysisId) {
        return Promise.resolve({ bundle, source: "khive-db-snapshot" });
      }
      return new Promise((resolveLoad) => {
        releaseDynamic = () => resolveLoad({
          bundle: dynamicBundle,
          source: "khive-db-snapshot",
        });
      });
    });
    const { container } = render(<Showcase />);
    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());

    const selector = screen.getByRole("combobox", { name: "Repository analysis" });
    expect(screen.getByLabelText("Public repository URL")).toBeVisible();
    expect(screen.getByRole("status", { name: "Repository catalog status" }))
      .toHaveTextContent("2 configured repository analyses discovered.");

    await user.selectOptions(selector, `analysis:${dynamicAnalysisId}`);

    expect(selector).toHaveAttribute("aria-busy", "true");
    expect(container.querySelector(".repo-result")).toHaveAttribute("aria-busy", "true");
    expect(screen.getByRole("status", { name: "Repository analysis status" }))
      .toHaveTextContent(/opening.*dynamic-only/i);
    releaseDynamic?.();

    await waitFor(() => expect(selector).toHaveAttribute("aria-busy", "false"));
    expect(screen.getByLabelText("Public repository URL")).toHaveValue(
      dynamicUrl,
    );
    expect(new URL(window.location.href).searchParams.get("repo")).toBe(
      dynamicUrl,
    );
    expect(screen.getByLabelText("Analysis source")).toHaveTextContent(
      "khive DB snapshot",
    );
  });

  it("drops foreign query parameters and the fragment from history on a successful initial resolution", async () => {
    const repo = bundle.meta.repository.canonical_url;
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent(repo)}&access_token=secret#fragment`,
    );

    const { container } = render(<Showcase />);

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    const url = new URL(window.location.href);
    expect(url.searchParams.get("repo")).toBe(repo);
    expect(url.searchParams.has("access_token")).toBe(false);
    expect(url.hash).toBe("");
    expect([...url.searchParams.keys()]).toEqual(
      expect.arrayContaining(["repo"]),
    );
    for (const key of url.searchParams.keys()) {
      expect(["repo", "at", "module", "view"]).toContain(key);
    }
  });

  it("drops foreign query parameters and the fragment from history for an invalid repository", async () => {
    window.history.replaceState(
      null,
      "",
      `/?repo=${
        encodeURIComponent("ftp://example.com/owner/repo")
      }&access_token=secret#fragment`,
    );

    render(<Showcase />);

    expect(await screen.findByText("Repository lookup could not start")).toBeVisible();
    const url = new URL(window.location.href);
    expect(url.searchParams.has("access_token")).toBe(false);
    expect(url.hash).toBe("");
    expect([...url.searchParams.keys()]).toEqual([]);
  });

  it("treats an empty repo parameter as invalid instead of falling back to the default entry", async () => {
    window.history.replaceState(
      null,
      "",
      "/?repo=&access_token=secret#fragment",
    );

    render(<Showcase />);

    expect(await screen.findByText("Repository lookup could not start")).toBeVisible();
    const url = new URL(window.location.href);
    expect(url.searchParams.has("access_token")).toBe(false);
    expect(url.hash).toBe("");
    expect([...url.searchParams.keys()]).toEqual([]);
  });

  it("treats duplicate repo parameters as invalid instead of resolving the first value", async () => {
    const repo = bundle.meta.repository.canonical_url;
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent(repo)}&repo=${
        encodeURIComponent(repo)
      }&access_token=secret#fragment`,
    );

    render(<Showcase />);

    expect(await screen.findByText("Repository lookup could not start")).toBeVisible();
    const url = new URL(window.location.href);
    expect(url.searchParams.get("repo")).toBe(repo);
    expect(url.searchParams.has("access_token")).toBe(false);
    expect(url.hash).toBe("");
    expect([...url.searchParams.keys()]).toEqual(["repo"]);
  });

  it("drops foreign query parameters, the fragment, and a credential nested inside the repo value from history for a registry miss", async () => {
    const repo = "https://github.com/example/not-catalog-registered";
    const nestedCredentialRepo = `${repo}?access_token=SECRET`;
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent(nestedCredentialRepo)}&access_token=TOP#frag`,
    );

    render(<Showcase />);

    expect(
      await screen.findByText("No curated showcase bundle matches this repository"),
    ).toBeVisible();
    const url = new URL(window.location.href);
    // The repo value is preserved minus its own nested query/fragment —
    // the credential inside it must not survive into history.
    expect(url.searchParams.get("repo")).toBe(repo);
    expect(url.searchParams.has("access_token")).toBe(false);
    expect(url.hash).toBe("");
    expect(url.href).not.toContain("SECRET");
    expect(url.href).not.toContain("TOP");
    expect([...url.searchParams.keys()]).toEqual(["repo"]);
  });

  it("drops a credential nested inside the repo value from history on the invalid branch", async () => {
    const nestedCredentialRepo = "ftp://example.com/owner/repo?access_token=SECRET";
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent(nestedCredentialRepo)}&access_token=TOP#frag`,
    );

    render(<Showcase />);

    expect(await screen.findByText("Repository lookup could not start")).toBeVisible();
    const url = new URL(window.location.href);
    expect(url.searchParams.has("access_token")).toBe(false);
    expect(url.hash).toBe("");
    expect(url.href).not.toContain("SECRET");
    expect(url.href).not.toContain("TOP");
    expect([...url.searchParams.keys()]).toEqual([]);
  });

  it("renders an honest miss when a dynamic-only analysis disappears", async () => {
    const dynamicUrl = "https://github.com/example/dynamic-only";
    const dynamicAnalysisId = "missing-only";
    mockedCatalog.mockResolvedValue({
      status: "ready",
      entries: [{
        analysis_id: dynamicAnalysisId,
        canonical_url: dynamicUrl,
      }],
      message: "1 configured repository analysis discovered.",
    });
    mockedLoad.mockRejectedValue(new ShowcaseAnalysisNotFoundError(dynamicUrl));
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent(dynamicUrl)}`,
    );

    const { container } = render(<Showcase />);

    await waitFor(() => expect(container.querySelector('[data-state="empty"]')).toBeVisible());
    expect(screen.getByText("Configured repository analysis is unavailable"))
      .toBeVisible();
    expect(container.querySelector('[data-state="empty"]')).toHaveTextContent(dynamicUrl);
    expect(mockedLoad).toHaveBeenCalledOnce();
  });
});
