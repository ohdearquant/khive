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
});
