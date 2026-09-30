import type { KeyboardEvent } from "react";

const ROW_SELECTOR = "[data-keyboard-row]";
const LIST_SELECTOR = "[data-keyboard-list], .repo-list, .repo-score-modules, tbody, ul, ol";
const EDITING_SELECTOR = "input, textarea, select, [contenteditable], [role='combobox']";

/** Move among visible, actionable rows in the current list or timeline. */
export function handleKeyboardRows(event: KeyboardEvent<HTMLElement>): void {
  if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) {
    return;
  }
  if (event.key !== "j" && event.key !== "k") return;
  const target = event.target;
  if (!(target instanceof HTMLElement) || target.closest(EDITING_SELECTOR)) return;
  if (target.closest("[role='dialog']")) return;

  const root = event.currentTarget;
  const scope = target.closest<HTMLElement>("[data-keyboard-scope]") ?? root;
  const list = target.closest<HTMLElement>(LIST_SELECTOR) ??
    (scope.matches(LIST_SELECTOR) && scope.querySelector(ROW_SELECTOR) ? scope :
      Array.from(scope.querySelectorAll<HTMLElement>(LIST_SELECTOR))
        .find((candidate) => candidate.querySelector(ROW_SELECTOR)));
  if (!list || !root.contains(list)) return;
  const rows = Array.from(list.querySelectorAll<HTMLElement>(ROW_SELECTOR))
    .filter((row) => !row.hidden && row.getAttribute("aria-hidden") !== "true" && !row.hasAttribute("disabled"));
  if (rows.length === 0) return;

  const current = target.closest<HTMLElement>(ROW_SELECTOR);
  const index = current ? rows.indexOf(current) : -1;
  const next = event.key === "j"
    ? rows[Math.min(index + 1, rows.length - 1)]
    : rows[index < 0 ? rows.length - 1 : Math.max(index - 1, 0)];
  if (!next) return;
  event.preventDefault();
  next.focus();
  next.scrollIntoView?.({ block: "nearest" });
  // Rows are native buttons or links: Enter activates their existing action.
}
