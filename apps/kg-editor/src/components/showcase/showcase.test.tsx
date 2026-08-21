import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { loadPreferredShowcaseBundle } from "@/lib/adapters/preferred-showcase-source";
import { parseRepoBundle } from "@/lib/repo-bundle";

vi.mock("@/lib/adapters/preferred-showcase-source", () => ({
  loadPreferredShowcaseBundle: vi.fn(),
  readOperatorShowcaseAccessToken: vi.fn(() => null),
}));

import { Showcase } from "@/components/showcase/showcase";

const goldenPath = resolve(process.cwd(), "../../docs/schemas/examples/khive-repo-v1-khive.json");
const bundle = parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
const mockedLoad = vi.mocked(loadPreferredShowcaseBundle);

describe("materialized repository lookup", () => {
  beforeEach(() => {
    window.history.replaceState(null, "", "/");
    mockedLoad.mockClear();
    mockedLoad.mockResolvedValue({ bundle, source: "khive-db-snapshot" });
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
  }, 15_000);

  it("serves the public static fallback from cache instead of reloading it", async () => {
    mockedLoad.mockClear();
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
});
