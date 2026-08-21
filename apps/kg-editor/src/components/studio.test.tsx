import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

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
    name: "checks",
    view: /^checks/i,
    empty: (bundle: ReviewBundle) => { bundle.checks.items = []; },
  },
  {
    name: "evidence",
    view: /provenance/i,
    empty: (bundle: ReviewBundle) => { bundle.evidence.items = []; },
  },
  {
    name: "retrieval",
    view: /khive context/i,
    empty: (bundle: ReviewBundle) => { bundle.retrieval.search.items = []; },
  },
  {
    name: "activity",
    view: /^activity/i,
    empty: (bundle: ReviewBundle) => { bundle.activity.items = []; },
  },
] as const;

describe("KG Studio", () => {
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
  ] as const)("does not render the checks success hero for a zero-item %s page", async (_name, incomplete) => {
    const bundle = structuredClone(demoReviewFixture);
    bundle.checks.items = [];
    bundle.checks.truncated = incomplete.truncated;
    bundle.checks.next_cursor = incomplete.next_cursor;
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={bundle} />);

    await user.click(screen.getAllByRole("button", { name: /^checks/i })[0]);

    const surface = container.querySelector<HTMLElement>(".review-surface")!;
    expect(within(surface).queryByText("No error-level findings")).not.toBeInTheDocument();
    expect(surface.querySelector('[data-state="empty"]')).not.toBeInTheDocument();
    expect(surface.querySelector('[data-state="truncated"]')).toBeVisible();
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

  it("uses explicit entity_kind in core review operation lists", async () => {
    const user = userEvent.setup();
    const operation = {
      ...demoReviewFixture.change_set.operations[0],
      after: { kind: "entity", entity_kind: "concept", name: "Canonical concept" },
    };
    const report: ReviewReport = {
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
    const serialized = JSON.stringify(report);
    const imported = new File([serialized], "core-review.json", { type: "application/json" });
    Object.defineProperty(imported, "text", { value: () => Promise.resolve(serialized) });
    const { container } = render(<Studio initialBundle={demoReviewFixture} />);

    await user.upload(container.querySelector<HTMLInputElement>('input[type="file"]')!, imported);

    expect(await screen.findByRole("heading", { name: "Attributed change-set review" })).toBeVisible();
    expect(container.querySelector('.core-operation-list [data-kind="concept"]')).toHaveTextContent("Concept");
  });

  it("refuses same-family approval and records no approval state", async () => {
    const user = userEvent.setup();
    render(<Studio initialBundle={demoReviewFixture} />);

    await user.click(screen.getByRole("button", { name: /Approve locally/i }));

    expect(
      screen.getAllByText(/ADR-102 requires a reviewer outside family:demo-frontier/i),
    ).toHaveLength(2);
    expect(screen.queryByText(/Local decision: approved/i)).not.toBeInTheDocument();
  });

  it("clears a local approval when reviewer-family eligibility changes", async () => {
    const user = userEvent.setup();
    render(<Studio initialBundle={demoReviewFixture} />);

    const reviewer = screen.getByRole("combobox", { name: "Reviewer model family" });
    await user.selectOptions(reviewer, "family:independent-reasoner");
    await user.click(screen.getByRole("button", { name: /Approve locally/i }));
    expect(screen.getByText(/Local decision: approved/i)).toBeVisible();

    await user.selectOptions(reviewer, "family:demo-frontier");
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

    await user.click(screen.getAllByRole("button", { name: /^Activity/i })[0]);
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
});
