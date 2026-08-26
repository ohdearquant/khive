import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it } from "vitest";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);

function golden(): RepoBundle {
  return parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
}

it("renders a nested structure-area treemap with disambiguated leaf labels", async () => {
  const bundle = golden();
  const user = userEvent.setup();
  const { container } = render(<RepoShowcase bundle={bundle} />);

  await user.click(screen.getByRole("button", {
    name: bundle.capability.views.structure_treemap.label,
  }));

  const treemap = container.querySelector<HTMLElement>(
    "[data-structure-treemap]",
  )!;
  expect(treemap).toHaveAttribute("data-area-metric", "source_file_count");
  expect(treemap.querySelectorAll("[data-treemap-package]").length)
    .toBeGreaterThan(1);
  expect(treemap.querySelectorAll("[data-treemap-directory]").length)
    .toBeGreaterThan(1);

  const packCards = [...treemap.querySelectorAll<HTMLButtonElement>(
    "button[data-module-id]",
  )].filter((button) =>
    button.querySelector("strong")?.textContent === "pack"
  );
  expect(packCards.length).toBeGreaterThan(1);
  const contexts = packCards.map((button) =>
    button.querySelector(".repo-treemap-context")?.textContent
  );
  expect(contexts.every(Boolean)).toBe(true);
  expect(new Set(contexts).size).toBe(packCards.length);
  expect(packCards.every((button) => Number(button.dataset.treemapWeight) > 0))
    .toBe(true);
});
