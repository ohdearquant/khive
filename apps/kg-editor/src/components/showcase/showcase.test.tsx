import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { loadStaticShowcaseBundle } from "@/lib/adapters/static-showcase-source";
import { parseRepoBundle } from "@/lib/repo-bundle";

vi.mock("@/lib/adapters/static-showcase-source", () => ({
  loadStaticShowcaseBundle: vi.fn(),
}));

import { Showcase } from "@/components/showcase/showcase";

const goldenPath = resolve(process.cwd(), "../../docs/schemas/examples/khive-repo-v1-khive.json");
const bundle = parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
const mockedLoad = vi.mocked(loadStaticShowcaseBundle);

describe("static repository lookup", () => {
  beforeEach(() => {
    window.history.replaceState(null, "", "/");
    mockedLoad.mockResolvedValue(bundle);
  });

  it("normalizes a curated alias and performs no bundle load for a later miss", async () => {
    const user = userEvent.setup();
    const { container } = render(<Showcase />);

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    expect(mockedLoad).toHaveBeenCalledTimes(1);
    expect(container.querySelector(".repo-overview")).toHaveAttribute(
      "data-head-sha",
      "c2979d2443738a075e55a170c772d1dc86cf0f91",
    );

    const input = screen.getByLabelText("Public repository URL");
    await user.clear(input);
    await user.type(input, "http://github.com/ohdearquant/khive.git");
    await user.click(screen.getByRole("button", { name: bundle.capability.labels.lookup_action }));

    await waitFor(() => expect(container.querySelector(".repo-overview")).toBeVisible());
    expect(window.location.search).toContain(
      "repo=https%3A%2F%2Fgithub.com%2Fohdearquant%2Fkhive",
    );
    expect(mockedLoad).toHaveBeenCalledTimes(1);

    await user.clear(input);
    await user.type(input, "https://github.com/example/not-curated");
    await user.click(screen.getByRole("button", { name: bundle.capability.labels.lookup_action }));

    expect(await screen.findByText(bundle.capability.labels.miss_title)).toBeVisible();
    expect(screen.getByText(new RegExp(bundle.capability.labels.miss_body))).toBeVisible();
    expect(window.location.search).toBe("");
    expect(mockedLoad).toHaveBeenCalledTimes(1);
  }, 15_000);
});
