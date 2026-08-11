import type { ReactNode } from "react";

import {
  Box,
  LoaderCircle,
  Search,
  XCircle,
  type IconComponent,
} from "@/icons";

type DataStateKind = "loading" | "empty" | "unavailable" | "error";

type DataStateBase = Readonly<{
  title: string;
  message: string;
  detail?: ReactNode;
  className?: string;
}>;

type DataStateProps = DataStateBase & (
  | Readonly<{
      state: "empty";
      action: Readonly<{ label: string; onClick: () => void }>;
    }>
  | Readonly<{
      state: Exclude<DataStateKind, "empty">;
      action?: never;
    }>
);

const stateIcons: Record<DataStateKind, IconComponent> = {
  loading: LoaderCircle,
  empty: Search,
  unavailable: Box,
  error: XCircle,
};

/**
 * One semantic rendering contract for bounded data surfaces. Empty collections
 * require one recovery action; non-empty states deliberately cannot grow
 * cosmetic or competing calls to action.
 */
export function DataState(props: DataStateProps) {
  const StateIcon = stateIcons[props.state];
  const classes = ["data-state", props.state, props.className].filter(Boolean).join(" ");

  return (
    <section
      className={classes}
      data-state={props.state}
      role={props.state === "error" ? "alert" : "status"}
      aria-busy={props.state === "loading"}
    >
      <StateIcon aria-hidden="true" />
      <div className="data-state-copy">
        <strong>{props.title}</strong>
        <span>{props.message}</span>
        {props.detail}
      </div>
      {props.state === "empty" && (
        <button
          className="button primary data-state-action"
          type="button"
          onClick={props.action.onClick}
        >
          {props.action.label}
        </button>
      )}
    </section>
  );
}
