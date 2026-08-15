import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { RepositoryCommandPalette } from "@/components/showcase/repository-command-palette";
import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";
import { REPOSITORY_VIEW_IDS } from "@/lib/repository-location";

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);

function golden(): RepoBundle {
  return parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
}

function renderPalette(
  overrides: Partial<React.ComponentProps<typeof RepositoryCommandPalette>> =
    {},
) {
  const bundle = golden();
  const props: React.ComponentProps<typeof RepositoryCommandPalette> = {
    bundle,
    activeView: "structure_graph",
    selectedModuleId: bundle.graph.modules.items[0]?.id ?? null,
    onSelectModule: vi.fn(),
    onSelectView: vi.fn(),
    onCopyLink: vi.fn(),
    ...overrides,
  };
  return {
    bundle,
    props,
    ...render(
      <div className="repo-shell">
        <div className="repo-meta-row">
          <RepositoryCommandPalette {...props} />
        </div>
      </div>,
    ),
  };
}

describe("repository command palette", () => {
  it("opens from either platform shortcut, exposes every view, and restores focus", async () => {
    const user = userEvent.setup();
    const { bundle } = renderPalette();
    const trigger = screen.getByRole("button", {
      name: "Open command palette",
    });
    trigger.focus();

    await user.keyboard("{Meta>}k{/Meta}");
    const dialog = screen.getByRole("dialog", { name: "Repository commands" });
    expect(dialog).toBeVisible();
    expect(dialog.closest(".repo-shell")).not.toBeNull();
    expect(dialog.closest(".repo-meta-row")).toBeNull();
    const query = screen.getByRole("combobox", {
      name: "Search repository commands",
    });
    expect(query).toHaveFocus();
    expect(within(dialog).getAllByRole("option")[0])
      .toHaveAttribute("aria-selected", "true");
    for (const viewId of REPOSITORY_VIEW_IDS) {
      expect(
        within(dialog).getByRole("option", {
          name: new RegExp(bundle.capability.views[viewId].label, "i"),
        }),
      ).toBeVisible();
    }
    await user.tab();
    expect(
      within(dialog).getByRole("button", {
        name: "Close command palette",
      }),
    ).toHaveFocus();
    await user.tab({ shift: true });
    expect(query).toHaveFocus();

    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog", { name: "Repository commands" }))
      .not.toBeInTheDocument();
    expect(trigger).toHaveFocus();

    await user.keyboard("{Control>}k{/Control}");
    expect(screen.getByRole("dialog", { name: "Repository commands" }))
      .toBeVisible();
    const repeatedShortcut = new KeyboardEvent("keydown", {
      key: "k",
      ctrlKey: true,
      repeat: true,
      cancelable: true,
    });
    window.dispatchEvent(repeatedShortcut);
    expect(repeatedShortcut.defaultPrevented).toBe(true);
    expect(screen.getByRole("dialog", { name: "Repository commands" }))
      .toBeVisible();
  });

  it("uses arrow traversal and Enter for bounded module, view, and copy commands", async () => {
    const bundle = golden();
    const writer = bundle.graph.modules.items.find((module) =>
      module.source_path.endsWith("khive-db/src/writer_task.rs")
    )!;
    const onSelectModule = vi.fn();
    const onSelectView = vi.fn();
    const onCopyLink = vi.fn();
    const scrollIntoView = vi.fn();
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
      configurable: true,
      value: scrollIntoView,
    });
    const user = userEvent.setup();
    renderPalette({ bundle, onSelectModule, onSelectView, onCopyLink });

    await user.keyboard("{Meta>}k{/Meta}");
    let query = screen.getByRole("combobox", {
      name: "Search repository commands",
    });
    const defaultOptions = screen.getAllByRole("option");
    expect(defaultOptions[0]).toHaveAttribute("aria-selected", "true");
    scrollIntoView.mockClear();
    await user.keyboard("{ArrowDown}");
    expect(defaultOptions[1]).toHaveAttribute("aria-selected", "true");
    expect(scrollIntoView).toHaveBeenLastCalledWith({ block: "nearest" });
    await user.keyboard("{Enter}");
    expect(onSelectView).toHaveBeenCalledWith(
      "history_structure_navigation",
    );

    await user.keyboard("{Meta>}k{/Meta}");
    query = screen.getByRole("combobox", {
      name: "Search repository commands",
    });
    await user.type(query, writer.source_path);
    expect(screen.getAllByRole("option")).toHaveLength(1);
    await user.keyboard("{Enter}");
    expect(onSelectModule).toHaveBeenCalledOnce();
    expect(onSelectModule).toHaveBeenCalledWith(writer.id);

    await user.keyboard("{Meta>}k{/Meta}");
    const copyOption = screen.getAllByRole("option").at(-1)!;
    await user.keyboard("{ArrowUp}");
    expect(copyOption).toHaveAttribute("aria-selected", "true");
    await user.keyboard("{Enter}");
    expect(onCopyLink).toHaveBeenCalledOnce();
  });

  it("keeps unavailable views addressable and explains their status", async () => {
    const original = golden();
    const bundle = {
      ...original,
      capability: {
        ...original.capability,
        views: {
          ...original.capability.views,
          scorecard: {
            ...original.capability.views.scorecard,
            status: "unavailable" as const,
            unavailable_reason: "Score inputs were not captured.",
          },
        },
      },
    };
    const onSelectView = vi.fn();
    const user = userEvent.setup();
    renderPalette({ bundle, onSelectView });

    await user.click(
      screen.getByRole("button", { name: "Open command palette" }),
    );
    const scorecard = screen.getByRole("option", { name: /scorecard/i });
    expect(scorecard).toHaveTextContent("Unavailable");
    expect(scorecard).toHaveTextContent("Score inputs were not captured.");
    await user.click(scorecard);
    expect(onSelectView).toHaveBeenCalledWith("scorecard");
  });

  it("discloses when module search only covers a bounded captured page", async () => {
    const bundle = structuredClone(golden());
    bundle.graph.modules.truncated = true;
    bundle.graph.modules.next_cursor = "next-module-page";
    bundle.graph.modules.total_count = {
      status: "available",
      value: bundle.graph.modules.items.length + 42,
    };
    bundle.graph.modules.disclosure = {
      status: "truncated",
      reason: "module page reached its export bound",
    };
    const user = userEvent.setup();
    renderPalette({ bundle });

    await user.click(
      screen.getByRole("button", { name: "Open command palette" }),
    );
    expect(screen.getByText(
      new RegExp(
        `${bundle.graph.modules.items.length} of ${
          bundle.graph.modules.items.length + 42
        } captured module records`,
        "i",
      ),
    )).toHaveTextContent(/Truncated.*module page reached its export bound/i);
    expect(screen.getByText(/Up to 8 module matches/i)).toBeVisible();
  });

  it("does not claim complete module search coverage when a next page exists", async () => {
    const bundle = structuredClone(golden());
    bundle.graph.modules.truncated = false;
    bundle.graph.modules.next_cursor = "next-module-page";
    bundle.graph.modules.disclosure = { status: "complete" };
    const user = userEvent.setup();
    renderPalette({ bundle });

    await user.click(
      screen.getByRole("button", { name: "Open command palette" }),
    );
    expect(screen.getByText(/captured module records/i))
      .toHaveTextContent(/Truncated/i);
    expect(screen.getByText(/captured module records/i))
      .not.toHaveTextContent(/complete/i);
  });
});
