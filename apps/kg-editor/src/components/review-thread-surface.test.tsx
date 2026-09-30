import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { ReviewThreadSurface } from "@/components/review-thread-surface";
import { demoReviewFixture } from "@/lib/fixtures/demo-review";
import type { ReviewReport } from "@/lib/review-bundle";
import { buildReviewThreadModel, reviewUnitKey, type ReviewAnnotation } from "@/lib/review-thread";

const conceptId = "a1f00000-0000-4000-8000-000000000001";
const sourceId = "a1f00000-0000-4000-8000-000000000002";

function eventKeys(thread: Element): string[] {
  return [...thread.querySelectorAll("[data-thread-event-key]")]
    .map((event) => event.getAttribute("data-thread-event-key") ?? "");
}

describe("review list and thread surface", () => {
  it("renders one PR unit row and its events in source-time order, with undated findings explicit", async () => {
    const conceptKey = reviewUnitKey("entity", conceptId);
    const annotations: ReviewAnnotation[] = [
      { id: "later", unitKey: conceptKey, actor: "actor:reviewer", body: "Later note", createdAt: "2026-08-07T18:00:00Z" },
      { id: "earlier", unitKey: conceptKey, actor: "actor:reviewer", body: "Earlier note", createdAt: "2026-08-07T15:00:00Z" },
    ];
    const onAddLocalNote = vi.fn();
    const { container, rerender } = render(
      <ReviewThreadSurface
        input={demoReviewFixture}
        selectedUnitKey={conceptKey}
        onSelectUnit={vi.fn()}
        annotations={annotations}
        draft="Another note"
        onDraft={vi.fn()}
        onAddLocalNote={onAddLocalNote}
      />,
    );
    const thread = container.querySelector("[data-review-thread]");
    expect(thread).not.toBeNull();
    expect(container.querySelectorAll("[data-review-unit-row]")).toHaveLength(
      buildReviewThreadModel(demoReviewFixture, annotations).units.length,
    );
    expect(eventKeys(thread!)).toEqual([
      "evidence:evidence-incident",
      "annotation:earlier",
      "evidence:evidence-prov-o",
      `operation:0:${conceptId}`,
      "annotation:later",
    ]);
    expect(within(thread as HTMLElement).getByText(/Batch staged · 2026-08-07T/)).toHaveAttribute("datetime");
    expect(within(thread as HTMLElement).getByText("Earlier note")).toBeVisible();
    expect(thread?.querySelector('[data-kind="concept"]')).not.toBeNull();
    expect(container.querySelector("[aria-label='Review material without a unit link']"))
      .toHaveTextContent("no subject ID");

    await userEvent.setup().click(screen.getByRole("button", { name: "Add local note" }));
    expect(onAddLocalNote).toHaveBeenCalledOnce();

    rerender(
      <ReviewThreadSurface
        input={demoReviewFixture}
        selectedUnitKey={reviewUnitKey("entity", sourceId)}
        onSelectUnit={vi.fn()}
        annotations={[]}
      />,
    );
    const sourceThread = container.querySelector("[data-review-thread]");
    expect(eventKeys(sourceThread!).at(-1)).toBe("finding:0:citation-date-lint");
    expect(within(sourceThread as HTMLElement).getByText("Finding time unavailable in review report")).toBeVisible();
    expect(sourceThread?.querySelector("[data-thread-event-kind='finding'] time")).toBeNull();
  });

  it("uses the same full-ID rows and selected thread for the changeset variant", async () => {
    const first = demoReviewFixture.change_set.operations[0];
    const noteId = "a1f00000-0000-4000-8000-0000000000aa";
    const edgeId = "a1f00000-0000-4000-8000-0000000000bb";
    const report: ReviewReport = {
      schema_version: "khive.review.v1",
      review_kind: "changeset",
      capability: {
        source: "cli",
        mutability: "read_only",
        no_writes: true,
        git_reads: false,
        khive_reads: false,
        github_writes: false,
        wasm: false,
        persistence: false,
        unavailable_actions: ["stage", "apply", "commit", "push", "publish"],
      },
      change_set: {
        envelope: demoReviewFixture.change_set.envelope,
        operations: [
          first,
          { ...first, index: 1, id: noteId, target: "note", op: "update", summary: "Update note" },
          { ...first, index: 2, id: edgeId, target: "edge", op: "link", summary: "Link edge" },
        ],
      },
      tier_summary: { ...demoReviewFixture.tier_summary, operations: 3, tier_1: 3, tier_2: 0, highest_tier: "tier_1" },
      validation: demoReviewFixture.validation,
      findings: [],
      review_gate: demoReviewFixture.review_gate,
    };
    const onSelectUnit = vi.fn();
    const noteKey = reviewUnitKey("note", noteId);
    const { container } = render(
      <ReviewThreadSurface input={report} selectedUnitKey={noteKey} onSelectUnit={onSelectUnit} annotations={[]} />,
    );
    const rows = [...container.querySelectorAll<HTMLButtonElement>("[data-review-unit-row]")];
    expect(rows.map((row) => row.dataset.unitKey)).toEqual([
      reviewUnitKey("entity", conceptId), noteKey, reviewUnitKey("edge", edgeId),
    ]);
    expect(rows.every((row) => row.querySelector("code.record-id")?.textContent === row.dataset.unitKey?.split(":")[1]))
      .toBe(true);
    expect(rows[1]).toHaveAttribute("aria-pressed", "true");
    expect(container.querySelector("[data-review-thread]"))
      .toHaveTextContent("Update note");
    expect(container.querySelector("[data-review-thread]"))
      .toHaveTextContent("This changeset report does not carry evidence enrichment.");

    await userEvent.setup().click(rows[2]);
    expect(onSelectUnit).toHaveBeenCalledWith(reviewUnitKey("edge", edgeId));
  });
});
