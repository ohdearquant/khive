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

  it("shows a semantic-only pull request unit's field paths with their before and after values", () => {
    const enrichmentId = "a1f00000-0000-4000-8000-0000000000ee";
    const input = {
      ...demoReviewFixture,
      changes: {
        ...demoReviewFixture.changes,
        items: [
          ...demoReviewFixture.changes.items,
          {
            id: enrichmentId,
            substrate: "entity" as const,
            change: "modified" as const,
            title: "Enrichment-only concept",
            subtitle: "concept · description rewrite",
            tier: "tier_1" as const,
            fields: [
              { path: "description", before: "Old description text", after: "New description text" },
              { path: "aliases", after: ["first-alias", "second-alias"] },
              { path: "retired_field", before: "legacy value" },
            ],
            evidence_ids: [],
          },
        ],
      },
    };
    const key = reviewUnitKey("entity", enrichmentId);
    const model = buildReviewThreadModel(input, []);
    expect(model.units.find((unit) => unit.key === key)?.operations).toHaveLength(0);
    const { container } = render(
      <ReviewThreadSurface input={input} selectedUnitKey={key} onSelectUnit={vi.fn()} annotations={[]} />,
    );
    const thread = container.querySelector<HTMLElement>("[data-review-thread]")!;
    expect(eventKeys(thread)).toEqual([]);
    const fields = thread.querySelector<HTMLElement>("[data-review-thread-fields]");
    expect(fields).not.toBeNull();
    const diff = (path: string) => fields!.querySelector<HTMLElement>(`.field-diff[data-field-path="${path}"]`)!;
    expect(diff("description").querySelector(".field-name")).toHaveTextContent("description");
    expect(diff("description").querySelector("pre.before")).toHaveTextContent("Old description text");
    expect(diff("description").querySelector("pre.after")).toHaveTextContent("New description text");
    expect(diff("aliases").querySelector("pre.before")).toBeNull();
    expect(diff("aliases").querySelector("pre.after")).toHaveTextContent(/"first-alias",\s*"second-alias"/);
    expect(diff("retired_field").querySelector("pre.before")).toHaveTextContent("legacy value");
    expect(diff("retired_field").querySelector("pre.after")).toBeNull();
  });

  it("renders an imported value nested past the stringify recursion limit without throwing", async () => {
    const VALUE_TOO_DEEP = "Value is nested too deeply to display.";
    const deep: Record<string, unknown> = {};
    let cursor = deep;
    for (let level = 0; level < 8000; level += 1) {
      const next: Record<string, unknown> = {};
      cursor.nested = next;
      cursor = next;
    }
    expect(() => JSON.stringify(deep, null, 2)).toThrow(RangeError);
    const changeId = "e8400000-0000-4000-8000-000000000031";
    const input = {
      ...demoReviewFixture,
      change_set: {
        ...demoReviewFixture.change_set,
        operations: demoReviewFixture.change_set.operations.map((operation) =>
          operation.id === changeId ? { ...operation, before: deep } : operation,
        ),
      },
      changes: {
        ...demoReviewFixture.changes,
        items: demoReviewFixture.changes.items.map((item) =>
          item.id === changeId ? { ...item, fields: [{ path: "weight", before: deep, after: 0.62 }] } : item,
        ),
      },
    };
    const key = reviewUnitKey("edge", changeId);
    const { container } = render(
      <ReviewThreadSurface input={input} selectedUnitKey={key} onSelectUnit={vi.fn()} annotations={[]} />,
    );
    const thread = container.querySelector<HTMLElement>("[data-review-thread]")!;
    const values = thread.querySelector<HTMLElement>(".review-thread-record-values")!;
    await userEvent.click(within(values).getByText("Record values"));
    expect(within(values).getByText("Before").parentElement!.querySelector("pre")).toHaveTextContent(VALUE_TOO_DEEP);
    expect(within(values).getByText("After").parentElement!.querySelector("pre")).toHaveTextContent('"weight": 0.62');
    const weight = thread.querySelector<HTMLElement>('[data-review-thread-fields] .field-diff[data-field-path="weight"]')!;
    expect(weight.querySelector("pre.before")).toHaveTextContent(VALUE_TOO_DEEP);
    expect(weight.querySelector("pre.after")).toHaveTextContent("0.62");
  });

  it("shows both the operation events and the semantic field diff for a unit that carries both", () => {
    const change = demoReviewFixture.changes.items.find((item) => item.id.startsWith("e8400000"))!;
    const key = reviewUnitKey(change.substrate, change.id);
    const { container } = render(
      <ReviewThreadSurface input={demoReviewFixture} selectedUnitKey={key} onSelectUnit={vi.fn()} annotations={[]} />,
    );
    const thread = container.querySelector<HTMLElement>("[data-review-thread]")!;
    expect(eventKeys(thread).some((event) => event.startsWith("operation:") && event.endsWith(change.id))).toBe(true);
    const weight = thread.querySelector<HTMLElement>('[data-review-thread-fields] .field-diff[data-field-path="weight"]');
    expect(weight).not.toBeNull();
    expect(weight!.querySelector("pre.before")).toHaveTextContent("0.84");
    expect(weight!.querySelector("pre.after")).toHaveTextContent("0.62");
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
