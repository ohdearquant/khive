import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { Studio } from "@/components/studio";
import { demoReviewFixture } from "@/lib/fixtures/demo-review";
import { REVIEW_IMPORT_MAX_BYTES, type ReviewBundle, type ReviewReport } from "@/lib/review-bundle";

const zeroPageCases = [
  {
    name: "affected graph",
    view: /affected graph/i,
    empty: (bundle: ReviewBundle) => {
      bundle.graph.nodes.items = [];
      bundle.graph.edges.items = [];
    },
  },
  {
    name: "retrieval",
    view: /khive context/i,
    empty: (bundle: ReviewBundle) => { bundle.retrieval.search.items = []; },
  },
] as const;

const emptyActivityFacets = [
  {
    name: "validation",
    facet: "Validation",
    empty: (bundle: ReviewBundle) => { bundle.checks.items = []; },
  },
  {
    name: "evidence",
    facet: "Evidence",
    empty: (bundle: ReviewBundle) => { bundle.evidence.items = []; },
  },
  {
    name: "conversation",
    facet: "Conversation",
    empty: (bundle: ReviewBundle) => { bundle.activity.items = []; },
  },
] as const;

function cliReviewReport(): ReviewReport {
  const operation = {
    ...demoReviewFixture.change_set.operations[0],
    after: { kind: "entity", entity_kind: "concept", name: "Canonical concept" },
  };
  return {
    schema_version: "khive.review.v1",
    review_kind: "changeset",
    capability: {
      source: "cli",
      mutability: "read_only",
      no_writes: true,
      git_reads: false,
      khive_reads: true,
      github_writes: false,
      wasm: false,
      persistence: false,
      unavailable_actions: ["apply", "commit", "push", "publish", "persist_review"],
    },
    change_set: { envelope: demoReviewFixture.change_set.envelope, operations: [operation] },
    tier_summary: {
      ...demoReviewFixture.tier_summary,
      operations: 1,
      tier_1: 1,
      tier_2: 0,
      highest_tier: "tier_1",
      requires_independent_review: false,
    },
    validation: demoReviewFixture.validation,
    findings: [],
    review_gate: demoReviewFixture.review_gate,
  };
}

describe("KG Studio", () => {
  beforeEach(() => window.history.replaceState(null, "", "/review"));

  it("makes the no-write and unavailable capability boundary visible", () => {
    render(<Studio initialBundle={demoReviewFixture} />);

    expect(screen.getByText("Demo data · no writes")).toBeVisible();
    expect(screen.getByText("WASM unavailable")).toBeVisible();
    expect(screen.getByText(/not persisted/i)).toBeVisible();
  });

  it("renders an actionable shared empty state for filtered graph changes", async () => {
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={demoReviewFixture} />);

    await user.type(screen.getByPlaceholderText("Filter entities, edges, tiers…"), "nothing-matches-this");

    const empty = container.querySelector<HTMLElement>('[data-state="empty"]');
    expect(empty).toBeVisible();
    expect(empty?.querySelectorAll("button")).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "Clear filter" }));
    expect(container.querySelector('[data-state="empty"]')).not.toBeInTheDocument();
  });

  it.each(zeroPageCases)("renders $name zero pages through one actionable empty state", async ({ view, empty }) => {
    const bundle = structuredClone(demoReviewFixture);
    empty(bundle);
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={bundle} />);

    await user.click(screen.getAllByRole("button", { name: view })[0]);

    const state = container.querySelector<HTMLElement>('[data-state="empty"]');
    expect(state).toBeVisible();
    expect(state?.querySelectorAll("button")).toHaveLength(1);
    expect(within(state!).getByRole("button", { name: "Import another review bundle" })).toBeVisible();
  });

  it.each(emptyActivityFacets)("shows an actionable empty $name facet in the shared timeline", async ({ facet, empty }) => {
    const bundle = structuredClone(demoReviewFixture);
    empty(bundle);
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={bundle} />);

    await user.click(screen.getByRole("tab", { name: /^Activity/i }));
    await user.click(screen.getByRole("button", { name: new RegExp(`^${facet}$`) }));

    const timeline = container.querySelector<HTMLElement>("[data-review-activity-timeline]");
    expect(timeline).toBeVisible();
    const state = timeline?.querySelector<HTMLElement>('[data-state="empty"]');
    expect(state).toBeVisible();
    await user.click(within(state!).getByRole("button", { name: "Show all activity" }));
    expect(container.querySelector("[data-review-activity-rows] [data-activity-kind]")).toBeInTheDocument();
  });

  it("renders Studio page bounds as the shared truncated state", () => {
    const bundle = structuredClone(demoReviewFixture);
    bundle.changes.truncated = true;
    bundle.changes.next_cursor = "next-page";
    const { container } = render(<Studio initialBundle={bundle} />);

    const state = container.querySelector<HTMLElement>('[data-state="truncated"]');
    expect(state).toHaveAttribute("data-shown", String(bundle.changes.items.length));
    expect(state).not.toHaveAttribute("data-bound");
    expect(state).toHaveTextContent(/graph changes.*bound unavailable/i);
  });

  it.each([
    ["truncated", { truncated: true, next_cursor: null }],
    ["next cursor", { truncated: false, next_cursor: "next-page" }],
  ] as const)("does not mislabel a zero-item %s page as known-empty", (_name, incomplete) => {
    const bundle = structuredClone(demoReviewFixture);
    bundle.changes.items = [];
    bundle.changes.truncated = incomplete.truncated;
    bundle.changes.next_cursor = incomplete.next_cursor;

    const { container } = render(<Studio initialBundle={bundle} />);

    expect(container.querySelector('[data-state="empty"]')).not.toBeInTheDocument();
    const state = container.querySelector<HTMLElement>('[data-state="truncated"]');
    expect(state).toBeVisible();
    expect(state).toHaveAttribute("data-shown", "0");
    expect(state).not.toHaveAttribute("data-bound");
  });

  it.each([
    ["truncated", { truncated: true, next_cursor: null }],
    ["next cursor", { truncated: false, next_cursor: "next-page" }],
  ] as const)("does not mislabel an incomplete zero-item validation facet as empty", async (_name, incomplete) => {
    const bundle = structuredClone(demoReviewFixture);
    bundle.checks.items = [];
    bundle.checks.truncated = incomplete.truncated;
    bundle.checks.next_cursor = incomplete.next_cursor;
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={bundle} />);

    await user.click(screen.getByRole("tab", { name: /^Activity/i }));
    await user.click(screen.getByRole("button", { name: /^Validation$/ }));

    const timeline = container.querySelector<HTMLElement>("[data-review-activity-timeline]")!;
    expect(timeline.querySelector('[data-state="empty"]')).not.toBeInTheDocument();
    expect(timeline.querySelector('[data-state="truncated"]')).toBeVisible();
  });

  it("navigates from semantic diff to the affected graph", async () => {
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={demoReviewFixture} />);

    await user.click(screen.getAllByRole("button", { name: /affected graph/i })[0]);
    expect(screen.getByRole("heading", { name: "Affected subgraph" })).toBeVisible();
    expect(screen.getAllByRole("button", { name: /Assertion-level provenance/i })[0]).toBeVisible();
    expect(screen.getByLabelText("Ontology legend")).toHaveTextContent(/Concept.*Document.*Dataset.*Project.*Person.*Organization.*Artifact.*Service.*Resource/i);
    expect(screen.getByLabelText("Ontology legend")).toHaveTextContent(/Derived/i);
    expect(container.querySelector('[data-kind="domain"]')).toHaveAttribute("title", "Unsupported kind: domain");
    expect(container.querySelector('line[data-edge-family="epistemic"]')).toBeInTheDocument();
    expect(container.querySelector('line[data-edge-family="epistemic"]')).toHaveAttribute("marker-end", "url(#studio-ontology-arrow)");
    expect(container.querySelector(".ontology-direction-glyph")).toHaveTextContent("›");
    expect(container.querySelector(".ontology-direction-glyph")?.getAttribute("transform")).toMatch(/^rotate\(/);
    expect(
      screen.getByRole("region", { name: "Affected graph relationships" }),
    ).toHaveTextContent(/introduced_by · 1\.00/i);
  });

  it("makes graph edges addressable with a shared selection and contextual inspector", async () => {
    const user = userEvent.setup();
    render(<Studio initialBundle={demoReviewFixture} />);

    await user.click(screen.getAllByRole("button", { name: /affected graph/i })[0]);
    const edgeSummaryRegion = screen.getByRole("region", { name: "Affected graph relationships" });
    const edgeRow = within(edgeSummaryRegion).getAllByRole("button")[0];

    edgeRow.focus();
    expect(edgeRow).toHaveFocus();

    await user.click(edgeRow);
    expect(edgeRow).toHaveAttribute("aria-pressed", "true");
    expect(document.querySelector(".edge-inspector")).toBeInTheDocument();
  });

  it("dispatches retrieval note kinds through the note legend", async () => {
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={demoReviewFixture} />);

    await user.click(screen.getAllByRole("button", { name: /Khive context/i })[0]);
    expect(container.querySelector('[data-kind="observation"]')).toHaveTextContent("Observation");
    expect(container.querySelector('[data-kind="observation"]')).not.toHaveTextContent("Unsupported kind");
  });

  it("preserves explicit entity_kind in the core review selected thread", async () => {
    const user = userEvent.setup();
    const report = cliReviewReport();
    const serialized = JSON.stringify(report);
    const imported = new File([serialized], "core-review.json", { type: "application/json" });
    Object.defineProperty(imported, "text", { value: () => Promise.resolve(serialized) });
    const { container } = render(<Studio initialBundle={demoReviewFixture} />);

    await user.upload(container.querySelector<HTMLInputElement>('input[type="file"]')!, imported);

    expect(await screen.findByRole("heading", { name: "Attributed change-set review" })).toBeVisible();
    expect(container.querySelectorAll("[data-review-unit-row]")).toHaveLength(1);
    const thread = container.querySelector<HTMLElement>("[data-review-thread]")!;
    expect(thread).toBeVisible();
    expect(thread.querySelector('[data-kind="concept"]')).toHaveTextContent("Concept");
    await user.click(within(thread).getByText("Record values"));
    expect(thread).toHaveTextContent(/"entity_kind": "concept"/);
  });

  it("keeps the underlying review decision, notes, and draft after a CLI report import", async () => {
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={demoReviewFixture} />);

    await user.click(screen.getByRole("button", { name: "Request changes" }));
    expect(screen.getByText(/Local decision: changes requested/i)).toBeVisible();
    const comment = screen.getByRole("textbox", { name: "Review comment" });
    await user.type(comment, "Keep this note");
    await user.click(screen.getByRole("button", { name: "Add local note" }));
    await user.type(comment, "Unsent draft");

    const serialized = JSON.stringify(cliReviewReport());
    const imported = new File([serialized], "core-review.json", { type: "application/json" });
    Object.defineProperty(imported, "text", { value: () => Promise.resolve(serialized) });
    await user.upload(container.querySelector<HTMLInputElement>('input[type="file"]')!, imported);
    expect(await screen.findByRole("heading", { name: "Attributed change-set review" })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Use demo review bundle" }));
    expect(screen.getByText(/Local decision: changes requested/i)).toBeVisible();
    expect(screen.getByText("Keep this note")).toBeVisible();
    expect(screen.getByRole("textbox", { name: "Review comment" })).toHaveValue("Unsent draft");
  });

  it("refuses same-family approval and records no approval state", async () => {
    const user = userEvent.setup();
    render(<Studio initialBundle={demoReviewFixture} />);

    await user.click(screen.getByRole("button", { name: /Approve locally/i }));

    expect(
      screen.getAllByText(/ADR-102 requires a reviewer outside family:demo-author/i),
    ).toHaveLength(2);
    expect(screen.queryByText(/Local decision: approved/i)).not.toBeInTheDocument();
  });

  it("clears a local approval when reviewer-family eligibility changes", async () => {
    const user = userEvent.setup();
    render(<Studio initialBundle={demoReviewFixture} />);

    const reviewer = screen.getByRole("combobox", { name: "Reviewer model family" });
    await user.selectOptions(reviewer, "family:demo-reviewer");
    await user.click(screen.getByRole("button", { name: /Approve locally/i }));
    expect(screen.getByText(/Local decision: approved/i)).toBeVisible();

    await user.selectOptions(reviewer, "family:demo-author");
    expect(screen.queryByText(/Local decision: approved/i)).not.toBeInTheDocument();
  });

  it("rejects an oversized review bundle before reading it", async () => {
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={demoReviewFixture} />);
    const input = container.querySelector<HTMLInputElement>('input[type="file"]');
    expect(input).not.toBeNull();

    const oversized = new File(
      [new Uint8Array(REVIEW_IMPORT_MAX_BYTES + 1)],
      "oversized-review.json",
      { type: "application/json" },
    );
    Object.defineProperty(oversized, "text", {
      value: () => Promise.reject(new Error("oversized file should not be read")),
    });

    await user.upload(input!, oversized);

    expect(await screen.findByText(/exceeds the 2 MiB local import limit/i)).toBeVisible();
  });

  it("resets local conversation notes when the imported review identity changes", async () => {
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={demoReviewFixture} />);

    await user.type(screen.getByRole("textbox", { name: "Review comment" }), "Only for review 184");
    await user.click(screen.getByRole("button", { name: "Add local note" }));
    expect(screen.getByText("Only for review 184")).toBeVisible();

    const nextBundle = {
      ...demoReviewFixture,
      repository: { ...demoReviewFixture.repository, head_sha: "1".repeat(40) },
      pull_request: {
        ...demoReviewFixture.pull_request,
        number: 185,
        head_sha: "1".repeat(40),
      },
    };
    const serialized = JSON.stringify(nextBundle);
    const imported = new File([serialized], "review-185.json", { type: "application/json" });
    Object.defineProperty(imported, "text", { value: () => Promise.resolve(serialized) });
    const input = container.querySelector<HTMLInputElement>('input[type="file"]');

    await user.upload(input!, imported);

    expect(await screen.findByText(/Loaded review bundle/i)).toBeVisible();
    expect(screen.queryByText("Only for review 184")).not.toBeInTheDocument();
  });

  it("uses the same command palette for review views and excludes approval actions", async () => {
    const user = userEvent.setup();
    render(<Studio initialBundle={demoReviewFixture} />);
    await user.keyboard("{Control>}k{/Control}");
    const dialog = screen.getByRole("dialog", { name: "Review commands" });
    expect(within(dialog).getByRole("option", { name: /Repository showcase/i }))
      .toBeVisible();
    expect(within(dialog).queryByRole("option", { name: /Approve locally/i }))
      .not.toBeInTheDocument();

    await user.type(screen.getByRole("combobox", { name: "Search review commands" }), "Activity");
    await user.keyboard("{Enter}");
    expect(screen.queryByRole("dialog", { name: "Review commands" }))
      .not.toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Activity" })).toBeVisible();
  });

  it("traverses actionable change rows with j/k and opens one with Enter", async () => {
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={demoReviewFixture} />);
    const rows = Array.from(container.querySelectorAll<HTMLButtonElement>(
      "[data-review-unit-row]",
    ));
    expect(rows.length).toBeGreaterThan(1);
    rows[0].focus();
    await user.keyboard("j");
    expect(rows[1]).toHaveFocus();
    await user.keyboard("k");
    expect(rows[0]).toHaveFocus();
    await user.keyboard("j{Enter}");
    expect(rows[1]).toHaveAttribute("aria-pressed", "true");
    expect(container.querySelector("[data-review-thread]")).toHaveTextContent(
      demoReviewFixture.changes.items[1].id,
    );
  });
});
