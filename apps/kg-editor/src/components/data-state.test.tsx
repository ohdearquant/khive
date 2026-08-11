import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { DataState } from "@/components/data-state";

describe("shared data states", () => {
  it.each([
    ["loading", "loader", "status"],
    ["unavailable", "box", "status"],
    ["error", "x-circle", "alert"],
  ] as const)("renders %s with its own icon and semantics", (state, icon, role) => {
    const { container } = render(
      <DataState
        state={state}
        title={`${state} title`}
        message={`${state} reason`}
      />,
    );

    const surface = screen.getByRole(role);
    expect(surface).toHaveAttribute("data-state", state);
    expect(surface).toHaveAttribute("aria-busy", state === "loading" ? "true" : "false");
    expect(container.querySelector(`[data-icon="${icon}"]`)).toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  it("gives a named empty collection exactly one primary action", async () => {
    const action = vi.fn();
    const user = userEvent.setup();
    const { container } = render(
      <DataState
        state="empty"
        title="No graph changes match this filter"
        message="Graph changes with the selected relation, entity kind, or tier belong here."
        action={{ label: "Clear filter", onClick: action }}
      />,
    );

    expect(screen.getByRole("status")).toHaveAttribute("data-state", "empty");
    expect(container.querySelector('[data-icon="search"]')).toBeInTheDocument();
    const buttons = screen.getAllByRole("button");
    expect(buttons).toHaveLength(1);
    expect(buttons[0]).toHaveClass("primary");
    await user.click(buttons[0]);
    expect(action).toHaveBeenCalledOnce();
  });

  it("states the truncation bound, reason, and known total without inventing an action", () => {
    const { container } = render(
      <DataState
        state="truncated"
        title="Graph nodes are truncated"
        shown={50}
        bound={50}
        knownTotal={143}
        reason="The review export applied its node budget."
      />,
    );

    const surface = screen.getByRole("status");
    expect(surface).toHaveAttribute("data-state", "truncated");
    expect(surface).toHaveAttribute("data-shown", "50");
    expect(surface).toHaveAttribute("data-bound", "50");
    expect(surface).toHaveAttribute("data-known-total", "143");
    expect(container.querySelector('[data-icon="alert-triangle"]')).toBeInTheDocument();
    expect(surface).toHaveTextContent(/50 shown.*bound 50.*143 total/i);
    expect(surface).toHaveTextContent("The review export applied its node budget.");
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  it("offers one next action only when it declares a wider bound", async () => {
    const action = vi.fn();
    const user = userEvent.setup();
    const { rerender } = render(
      <DataState
        state="truncated"
        title="Commits are truncated"
        shown={25}
        bound={25}
        reason="A page remains."
        next={{ bound: 50, label: "Show 50 commits", onClick: action }}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Show 50 commits" }));
    expect(action).toHaveBeenCalledOnce();

    rerender(
      <DataState
        state="truncated"
        title="Commits are truncated"
        shown={25}
        bound={25}
        reason="No wider page is available."
        next={{ bound: 25, label: "Invalid same bound", onClick: action }}
      />,
    );
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });
});

if (false) {
  // @ts-expect-error Non-empty states cannot grow an untyped second action.
  <DataState state="unavailable" title="Unavailable" message="Reason" action={{ label: "Retry", onClick() {} }} />;
  // @ts-expect-error Arbitrary interactive detail is outside the state contract.
  <DataState state="loading" title="Loading" message="Waiting" detail={<button type="button">Extra</button>} />;
}
