import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { ReviewActivityTimeline } from "@/components/review-activity-timeline";
import { demoReviewFixture } from "@/lib/fixtures/demo-review";

describe("review history facets", () => {
  it("filters one timeline in place without changing the route or losing other types", async () => {
    window.history.replaceState(null, "", "/review");
    const user = userEvent.setup();
    const { container } = render(
      <ReviewActivityTimeline input={demoReviewFixture} selectedEventId="" onSelectEvent={() => undefined} />,
    );
    const timeline = container.querySelector("[data-review-activity-timeline]");
    const rows = within(timeline as HTMLElement).getAllByRole("article");
    const allCount = rows.length;

    await user.click(screen.getByRole("button", { name: "Evidence" }));
    expect(container.querySelector("[data-review-activity-timeline]")).toBe(timeline);
    const evidence = container.querySelectorAll("[data-review-activity-rows] [data-activity-kind]");
    expect(evidence.length).toBeGreaterThan(0);
    expect(evidence.length).toBeLessThan(allCount);
    expect([...evidence].every((row) => row.getAttribute("data-activity-kind") === "evidence")).toBe(true);
    expect(window.location.pathname).toBe("/review");

    await user.click(screen.getByRole("button", { name: "All activity" }));
    expect(container.querySelectorAll("[data-review-activity-rows] [data-activity-kind]")).toHaveLength(allCount);
  });
});
