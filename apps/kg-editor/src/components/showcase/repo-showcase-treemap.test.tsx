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

it("reserves pixel body bands so overlay labels never cover child tiles", async () => {
  // jsdom cannot compute external CSS or container queries, so this guards
  // the stylesheet contract structurally, at both levels of the hierarchy:
  // children render inside a body element whose pixel inset starts below the
  // fixed-position label, and tiles too short for the band hide the label
  // and give the body the full tile.
  const css = readFileSync(
    resolve(process.cwd(), "src/app/showcase.css"),
    "utf8",
  );
  for (
    const { tile, body, label, labelTop } of [
      {
        tile: "repo-treemap-package",
        body: "repo-treemap-package-body",
        label: "repo-treemap-package-label",
        labelTop: 3,
      },
      {
        tile: "repo-treemap-directory",
        body: "repo-treemap-directory-body",
        label: "repo-treemap-directory-label",
        labelTop: 20,
      },
    ]
  ) {
    const tileRule = css.match(new RegExp(`\\.${tile}\\s*\\{[^}]*\\}`))?.[0];
    expect(tileRule, tile).toContain("container-type: size");
    const bodyRule = css.match(new RegExp(`\\.${body}\\s*\\{[^}]*\\}`))?.[0];
    const band = Number(bodyRule?.match(/inset: (\d+)px 0 0 0/)?.[1]);
    // The band must start below the label's fixed offset with room for the
    // label box itself.
    expect(band, body).toBeGreaterThanOrEqual(labelTop + 16);
    const suppression = css.match(
      new RegExp(
        `@container \\(max-height: (\\d+)px\\)\\s*\\{\\s*\\.${label}\\s*\\{[^}]*\\}\\s*\\.${body}\\s*\\{[^}]*\\}\\s*\\}`,
      ),
    );
    expect(suppression?.[0], label).toContain("display: none");
    expect(suppression?.[0], body).toContain("inset: 0");
    // Below the cutoff the full-bleed body takes over, so the band only ever
    // applies to tiles at least as tall as itself plus usable child space.
    expect(Number(suppression?.[1]), tile).toBeGreaterThanOrEqual(band + 24);
  }

  // The rendered hierarchy actually routes children through the body bands.
  const bundle = golden();
  const user = userEvent.setup();
  const { container } = render(<RepoShowcase bundle={bundle} />);
  await user.click(screen.getByRole("button", {
    name: bundle.capability.views.structure_treemap.label,
  }));
  const treemap = container.querySelector<HTMLElement>(
    "[data-structure-treemap]",
  )!;
  const directory = treemap.querySelector("[data-treemap-directory]")!;
  expect(directory.parentElement).toHaveClass("repo-treemap-package-body");
  expect(
    directory.querySelector("button[data-module-id]")?.parentElement,
  ).toHaveClass("repo-treemap-directory-body");
});
