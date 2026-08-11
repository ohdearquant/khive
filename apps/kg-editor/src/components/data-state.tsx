import {
  AlertTriangle,
  Box,
  LoaderCircle,
  Search,
  XCircle,
  type IconComponent,
} from "@/icons";

type DataStateKind = "loading" | "empty" | "unavailable" | "truncated" | "error";

type DataStateBase = Readonly<{
  title: string;
  className?: string;
  presentation?: "panel" | "inline";
  /** Plain, non-interactive context only; actions belong to the typed state variants. */
  context?: readonly string[];
}>;

type DataStateProps = DataStateBase & (
  | Readonly<{
      state: "empty";
      message: string;
      action: Readonly<{ label: string; onClick: () => void }>;
    }>
  | Readonly<{
      state: "loading" | "unavailable" | "error";
      message: string;
      action?: never;
    }>
  | Readonly<{
      state: "truncated";
      shown: number;
      /** Omit when the producer stopped on a non-row budget or did not disclose a row bound. */
      bound?: number;
      knownTotal?: number;
      reason: string;
      next?: Readonly<{ bound: number; label: string; onClick: () => void }>;
      message?: never;
      action?: never;
    }>
);

const stateIcons: Record<DataStateKind, IconComponent> = {
  loading: LoaderCircle,
  empty: Search,
  unavailable: Box,
  truncated: AlertTriangle,
  error: XCircle,
};

/**
 * One semantic rendering contract for bounded data surfaces. Empty collections
 * require one recovery action; truncated collections may offer one wider bound.
 * Every other state is deliberately non-interactive.
 */
export function DataState(props: DataStateProps) {
  const StateIcon = stateIcons[props.state];
  const classes = ["data-state", props.state, props.presentation === "inline" && "inline", props.className]
    .filter(Boolean)
    .join(" ");
  const inline = props.presentation === "inline";
  const canShowNext = props.state === "truncated"
    && props.bound !== undefined
    && props.next !== undefined
    && props.next.bound > props.bound;
  const Root = inline ? "span" : "section";

  return (
    <Root
      className={classes}
      data-state={props.state}
      data-shown={props.state === "truncated" ? props.shown : undefined}
      data-bound={props.state === "truncated" ? props.bound : undefined}
      data-known-total={props.state === "truncated" ? props.knownTotal : undefined}
      role={props.state === "error" ? "alert" : "status"}
      aria-busy={props.state === "loading"}
    >
      <StateIcon aria-hidden="true" />
      <span className="data-state-copy">
        <strong>{props.title}</strong>
        {props.state === "truncated" ? (
          <>
            <span>{props.reason}</span>
            <span className="data-state-bound">
              {props.shown} shown · {props.bound === undefined ? "bound unavailable" : `bound ${props.bound}`}
              {props.knownTotal === undefined ? " · total unavailable" : ` · ${props.knownTotal} total`}
            </span>
          </>
        ) : (
          <span>{props.message}</span>
        )}
        {props.context?.map((value) => <code key={value}>{value}</code>)}
      </span>
      {props.state === "empty" && (
        <button
          className="button primary data-state-action"
          type="button"
          onClick={props.action.onClick}
        >
          {props.action.label}
        </button>
      )}
      {canShowNext && props.state === "truncated" && props.next && (
        <button
          className="button primary data-state-action"
          type="button"
          onClick={props.next.onClick}
        >
          {props.next.label}
        </button>
      )}
    </Root>
  );
}
