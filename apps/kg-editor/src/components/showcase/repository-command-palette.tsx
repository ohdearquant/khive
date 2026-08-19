"use client";

import { Copy, Search, X } from "@/icons";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";

import styles from "@/components/showcase/repository-command-palette.module.css";
import { findRepositoryModules } from "@/lib/repository-brief";
import type { RepoBundle, RepoModule, ViewId } from "@/lib/repo-bundle";
import { REPOSITORY_VIEW_IDS } from "@/lib/repository-location";

const MODULE_RESULT_LIMIT = 8;

type PaletteCommand =
  | Readonly<{
    id: string;
    kind: "view";
    label: string;
    detail: string;
    view: ViewId;
  }>
  | Readonly<{
    id: string;
    kind: "module";
    label: string;
    detail: string;
    module: RepoModule;
  }>
  | Readonly<{
    id: "action:copy-link";
    kind: "action";
    label: "Copy investigation link";
    detail: "Copy the current repository, snapshot, module, and view.";
  }>;

export type RepositoryCommandPaletteProps = Readonly<{
  bundle: RepoBundle;
  activeView: ViewId;
  selectedModuleId: string | null;
  onSelectModule: (moduleId: string) => void;
  onSelectView: (view: ViewId) => void;
  onCopyLink: () => void | Promise<void>;
}>;

function includesQuery(command: PaletteCommand, query: string): boolean {
  const haystack = `${command.label} ${command.detail}`.toLowerCase();
  return query.split(/\s+/u).every((token) => haystack.includes(token));
}

export function RepositoryCommandPalette({
  bundle,
  activeView,
  selectedModuleId,
  onSelectModule,
  onSelectView,
  onCopyLink,
}: RepositoryCommandPaletteProps) {
  const [open, setOpen] = useState(false);
  const [portalTarget, setPortalTarget] = useState<HTMLElement | null>(null);
  const [query, setQuery] = useState("");
  const [highlightedIndex, setHighlightedIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const optionRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const returnFocusRef = useRef<HTMLElement | null>(null);
  const listboxId = useId();
  const dialogTitleId = useId();

  const commands = useMemo(() => {
    const normalizedQuery = query.trim().toLowerCase();
    const viewCommands: PaletteCommand[] = REPOSITORY_VIEW_IDS.map((view) => {
      const capability = bundle.capability.views[view];
      const availability = capability.status === "unavailable"
        ? `${bundle.capability.labels.unavailable}. ${
          capability.unavailable_reason ?? "No reason was supplied."
        }`
        : "Analysis view";
      return {
        id: `view:${view}`,
        kind: "view" as const,
        label: capability.label,
        detail: availability,
        view,
      };
    }).filter((command) =>
      !normalizedQuery || includesQuery(command, normalizedQuery)
    );
    const moduleCommands: PaletteCommand[] = normalizedQuery
      ? findRepositoryModules(bundle, normalizedQuery, MODULE_RESULT_LIMIT).items
        .map(
          (module) => ({
            id: `module:${module.id}`,
            kind: "module" as const,
            label: module.source_path,
            detail: module.module_path,
            module,
          }),
        )
      : [];
    const copyCommand: PaletteCommand = {
      id: "action:copy-link",
      kind: "action",
      label: "Copy investigation link",
      detail: "Copy the current repository, snapshot, module, and view.",
    };
    const actionCommands = !normalizedQuery ||
        includesQuery(copyCommand, normalizedQuery)
      ? [copyCommand]
      : [];
    return [...viewCommands, ...moduleCommands, ...actionCommands];
  }, [bundle, query]);
  const modulePage = bundle.graph.modules;
  const moduleLabel = bundle.capability.labels.node_types.module.toLowerCase();
  const moduleTotal = modulePage.total_count.status === "available"
    ? modulePage.total_count.value
    : null;
  const moduleSearchScope = `${modulePage.items.length}${
    moduleTotal != null && moduleTotal !== modulePage.items.length
      ? ` of ${moduleTotal}`
      : ""
  } captured ${moduleLabel} records · ${
    modulePage.disclosure.status === "complete" &&
      !modulePage.truncated &&
      modulePage.next_cursor == null
      ? "complete"
      : modulePage.disclosure.status === "unavailable"
      ? bundle.capability.labels.unavailable
      : bundle.capability.labels.truncated
  }${modulePage.disclosure.reason ? `: ${modulePage.disclosure.reason}` : ""}`;

  function openPalette(source?: HTMLElement | null) {
    returnFocusRef.current = source ??
      (document.activeElement instanceof HTMLElement
        ? document.activeElement
        : null);
    setQuery("");
    setHighlightedIndex(0);
    setPortalTarget(
      source?.closest<HTMLElement>(".repo-shell") ??
        triggerRef.current?.closest<HTMLElement>(".repo-shell") ??
        document.body,
    );
    setOpen(true);
  }

  function closePalette(restoreFocus = true) {
    setOpen(false);
    setQuery("");
    setHighlightedIndex(0);
    if (restoreFocus) {
      queueMicrotask(() => {
        if (returnFocusRef.current?.isConnected) returnFocusRef.current.focus();
      });
    }
  }

  function execute(command: PaletteCommand | undefined) {
    if (!command) return;
    if (command.kind === "view") {
      closePalette(false);
      onSelectView(command.view);
    } else if (command.kind === "module") {
      closePalette(false);
      onSelectModule(command.module.id);
    } else {
      closePalette();
      void onCopyLink();
    }
  }

  useEffect(() => {
    function handleShortcut(event: KeyboardEvent) {
      if (
        event.key.toLowerCase() === "k" &&
        (event.metaKey || event.ctrlKey) &&
        !event.altKey
      ) {
        event.preventDefault();
        if (event.repeat) return;
        if (open) closePalette();
        else openPalette();
      } else if (open && event.key === "Escape") {
        event.preventDefault();
        closePalette();
      }
    }
    window.addEventListener("keydown", handleShortcut);
    return () => window.removeEventListener("keydown", handleShortcut);
  }, [open]);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  const safeHighlightedIndex = commands.length
    ? Math.min(highlightedIndex, commands.length - 1)
    : 0;

  useEffect(() => {
    if (!open || commands.length === 0) return;
    optionRefs.current[safeHighlightedIndex]?.scrollIntoView?.({
      block: "nearest",
    });
  }, [commands.length, open, query, safeHighlightedIndex]);

  function handleInputKeyDown(event: React.KeyboardEvent<HTMLInputElement>) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setHighlightedIndex((index) =>
        commands.length ? (index + 1) % commands.length : 0
      );
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setHighlightedIndex((index) =>
        commands.length ? (index - 1 + commands.length) % commands.length : 0
      );
    } else if (event.key === "Home") {
      event.preventDefault();
      setHighlightedIndex(0);
    } else if (event.key === "End") {
      event.preventDefault();
      setHighlightedIndex(Math.max(0, commands.length - 1));
    } else if (event.key === "Enter") {
      event.preventDefault();
      execute(commands[safeHighlightedIndex]);
    }
  }

  function trapDialogFocus(event: React.KeyboardEvent<HTMLDivElement>) {
    if (event.key !== "Tab") return;
    if (!event.shiftKey && document.activeElement === inputRef.current) {
      event.preventDefault();
      closeRef.current?.focus();
    } else if (event.shiftKey && document.activeElement === closeRef.current) {
      event.preventDefault();
      inputRef.current?.focus();
    }
  }

  const highlightedCommand = commands[safeHighlightedIndex];
  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        className={styles.trigger}
        aria-label="Open command palette"
        aria-keyshortcuts="Meta+K Control+K"
        onClick={(event) => openPalette(event.currentTarget)}
      >
        <Search aria-hidden="true" />
        <span>Commands</span>
        <kbd>⌘/Ctrl K</kbd>
        <span className="visually-hidden">Open command palette</span>
      </button>
      {open && portalTarget && createPortal(
        (
          <div
            className={styles.backdrop}
            onMouseDown={(event) => {
              if (event.target === event.currentTarget) closePalette();
            }}
          >
            <div
              className={styles.dialog}
              role="dialog"
              aria-modal="true"
              aria-labelledby={dialogTitleId}
              onKeyDown={trapDialogFocus}
            >
              <header className={styles.header}>
                <div>
                  <span className={styles.eyebrow}>Repository navigation</span>
                  <h2 id={dialogTitleId}>Repository commands</h2>
                </div>
                <button
                  ref={closeRef}
                  type="button"
                  className={styles.close}
                  onClick={() => closePalette()}
                >
                  <X aria-hidden="true" />
                  <span className="visually-hidden">Close command palette</span>
                </button>
              </header>
              <label className={styles.search}>
                <Search aria-hidden="true" />
                <span className="visually-hidden">
                  Search repository commands
                </span>
                <input
                  ref={inputRef}
                  role="combobox"
                  aria-label="Search repository commands"
                  aria-expanded="true"
                  aria-controls={listboxId}
                  aria-activedescendant={highlightedCommand
                    ? `${listboxId}-${safeHighlightedIndex}`
                    : undefined}
                  autoComplete="off"
                  value={query}
                  placeholder="Jump to a module, view, or action"
                  onChange={(event) => {
                    setQuery(event.target.value);
                    setHighlightedIndex(0);
                  }}
                  onKeyDown={handleInputKeyDown}
                />
                <kbd>Esc</kbd>
              </label>
              <div className={styles.summary} aria-live="polite">
                {commands.length}{" "}
                {commands.length === 1 ? "command" : "commands"}
                {query.trim() ? " matched" : " available"}
              </div>
              <div
                id={listboxId}
                className={styles.results}
                role="listbox"
                aria-label="Repository command results"
              >
                {commands.map((command, index) => {
                  const current = command.kind === "view"
                    ? command.view === activeView
                    : command.kind === "module"
                    ? command.module.id === selectedModuleId
                    : false;
                  return (
                    <button
                      key={command.id}
                      ref={(node) => {
                        optionRefs.current[index] = node;
                      }}
                      id={`${listboxId}-${index}`}
                      type="button"
                      role="option"
                      tabIndex={-1}
                      aria-selected={index === safeHighlightedIndex}
                      className={styles.result}
                      data-highlighted={index === safeHighlightedIndex ||
                        undefined}
                      onMouseMove={() => setHighlightedIndex(index)}
                      onClick={() => execute(command)}
                    >
                      <span className={styles.resultIcon} aria-hidden="true">
                        {command.kind === "action"
                          ? <Copy />
                          : command.kind === "module"
                          ? "M"
                          : "V"}
                      </span>
                      <span className={styles.resultCopy}>
                        <strong>{command.label}</strong>
                        <small>{command.detail}</small>
                      </span>
                      <span className={styles.resultMeta}>
                        {current ? "Current" : command.kind}
                      </span>
                    </button>
                  );
                })}
                {commands.length === 0 && (
                  <p className={styles.empty}>
                    No repository command matches this query.
                  </p>
                )}
              </div>
              <footer className={styles.footer}>
                <span>
                  <kbd>↑</kbd>
                  <kbd>↓</kbd> Navigate
                </span>
                <span>
                  <kbd>↵</kbd> Open
                </span>
                <span>Up to {MODULE_RESULT_LIMIT} module matches</span>
                <span className={styles.scope}>{moduleSearchScope}</span>
              </footer>
            </div>
          </div>
        ),
        portalTarget,
      )}
    </>
  );
}
