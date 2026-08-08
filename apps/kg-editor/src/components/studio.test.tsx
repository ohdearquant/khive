import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { Studio } from "@/components/studio";
import { atlasReviewFixture } from "@/lib/fixtures/atlas-review";
import { REVIEW_IMPORT_MAX_BYTES } from "@/lib/review-bundle";

describe("KG Studio", () => {
  it("makes the no-write and unavailable capability boundary visible", () => {
    render(<Studio initialBundle={atlasReviewFixture} />);

    expect(screen.getByText("Demo data · no writes")).toBeVisible();
    expect(screen.getByText("WASM unavailable")).toBeVisible();
    expect(screen.getByText(/not persisted/i)).toBeVisible();
  });

  it("navigates from semantic diff to the affected graph", async () => {
    const user = userEvent.setup();
    render(<Studio initialBundle={atlasReviewFixture} />);

    await user.click(screen.getAllByRole("button", { name: /affected graph/i })[0]);
    expect(screen.getByRole("heading", { name: "Affected subgraph" })).toBeVisible();
    expect(screen.getByRole("button", { name: /Assertion-level provenance/i })).toBeVisible();
    expect(
      screen.getByRole("region", { name: "Affected graph relationships" }),
    ).toHaveTextContent(/introduced_by · 1\.00/i);
  });

  it("refuses same-family approval and records no approval state", async () => {
    const user = userEvent.setup();
    render(<Studio initialBundle={atlasReviewFixture} />);

    await user.click(screen.getByRole("button", { name: /Approve locally/i }));

    expect(
      screen.getAllByText(/ADR-102 requires a reviewer outside family:atlas-frontier/i),
    ).toHaveLength(2);
    expect(screen.queryByText(/Local decision: approved/i)).not.toBeInTheDocument();
  });

  it("clears a local approval when reviewer-family eligibility changes", async () => {
    const user = userEvent.setup();
    render(<Studio initialBundle={atlasReviewFixture} />);

    const reviewer = screen.getByRole("combobox", { name: "Reviewer model family" });
    await user.selectOptions(reviewer, "family:independent-reasoner");
    await user.click(screen.getByRole("button", { name: /Approve locally/i }));
    expect(screen.getByText(/Local decision: approved/i)).toBeVisible();

    await user.selectOptions(reviewer, "family:atlas-frontier");
    expect(screen.queryByText(/Local decision: approved/i)).not.toBeInTheDocument();
  });

  it("rejects an oversized review bundle before reading it", async () => {
    const user = userEvent.setup();
    const { container } = render(<Studio initialBundle={atlasReviewFixture} />);
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
    const { container } = render(<Studio initialBundle={atlasReviewFixture} />);

    await user.click(screen.getAllByRole("button", { name: /^Activity/i })[0]);
    await user.type(screen.getByRole("textbox", { name: "Review comment" }), "Only for review 184");
    await user.click(screen.getByRole("button", { name: "Add local note" }));
    expect(screen.getByText("Only for review 184")).toBeVisible();

    const nextBundle = {
      ...atlasReviewFixture,
      repository: { ...atlasReviewFixture.repository, head_sha: "1".repeat(40) },
      pull_request: {
        ...atlasReviewFixture.pull_request,
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
