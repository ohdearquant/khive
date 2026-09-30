"use client";

import {
  Activity,
  AlertTriangle,
  ArrowRight,
  Bot,
  Box,
  Brain,
  Check,
  CheckCircle2,
  ChevronDown,
  Circle,
  Clock3,
  Copy,
  Database,
  Download,
  FileJson2,
  FileText,
  GitBranch,
  GitCommitHorizontal,
  GitFork,
  GitPullRequest,
  Info,
  LockKeyhole,
  Menu,
  Network,
  Search,
  ShieldCheck,
  Sparkles,
  Upload,
  X,
  XCircle,
} from "@/icons";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";

import {
  edgeDirectionMark,
  edgeHueStyle,
  EntityKindMark,
  kindHueStyle,
  OntologyKindMark,
  OntologyLegend,
  RelationMark,
} from "@/components/ontology-mark";
import { DataState } from "@/components/data-state";
import { ReviewActivityTimeline } from "@/components/review-activity-timeline";
import { ReviewThreadSurface } from "@/components/review-thread-surface";
import { RepositoryCommandPalette } from "@/components/showcase/repository-command-palette";
import { settleGraphLayout } from "@/lib/graph-layout";
import { handleKeyboardRows } from "@/lib/keyboard-rows";
import { edgeLegendFor, entityLegendFor } from "@/lib/ontology-legend";
import { reviewActivityEvents } from "@/lib/review-activity";
import {
  isReviewReport,
  parseReviewInput,
  REVIEW_IMPORT_MAX_BYTES,
  type ReviewBundle,
  type ReviewInput,
  type ReviewReport,
} from "@/lib/review-bundle";
import {
  canApproveReview,
  shortHash,
  type ReviewDecision,
} from "@/lib/review-utils";
import { buildReviewThreadModel, type ReviewAnnotation } from "@/lib/review-thread";

type View = "changes" | "graph" | "retrieval" | "activity";
type Toast = { tone: "success" | "warning" | "neutral"; message: string } | null;

const viewLabels: Record<View, string> = {
  changes: "Changes",
  graph: "Affected graph",
  retrieval: "Khive context",
  activity: "Activity",
};

type GraphSelection = { type: "node" | "edge"; id: string };
type ReviewLocation = {
  view: View;
  selectedUnitKey: string;
  selectedEventId: string;
  graphSelection: GraphSelection;
};

function defaultReviewLocation(input: ReviewInput): ReviewLocation {
  return {
    view: "changes",
    selectedUnitKey: buildReviewThreadModel(input).units[0]?.key ?? "",
    selectedEventId: reviewActivityEvents(input)[0]?.id ?? "",
    graphSelection: { type: "node", id: input.review_kind === "pull_request" ? input.graph.nodes.items[0]?.id ?? "" : "" },
  };
}

function reviewLocationFromUrl(url: URL, input: ReviewInput): ReviewLocation {
  const fallback = defaultReviewLocation(input);
  const requestedView = url.searchParams.get("view");
  const view = requestedView !== null && Object.prototype.hasOwnProperty.call(viewLabels, requestedView) &&
    (input.review_kind === "pull_request" || requestedView === "changes" || requestedView === "activity")
    ? requestedView as View
    : fallback.view;
  const requestedUnit = url.searchParams.get("unit");
  let selectedUnitKey = fallback.selectedUnitKey;
  if (requestedUnit !== null && buildReviewThreadModel(input).units.some((unit) => unit.key === requestedUnit)) {
    selectedUnitKey = requestedUnit;
  }
  const requestedEvent = url.searchParams.get("event");
  let selectedEventId = fallback.selectedEventId;
  if (requestedEvent !== null && (
    requestedEvent === "" || reviewActivityEvents(input).some((event) => event.id === requestedEvent)
  )) selectedEventId = requestedEvent;

  let graphSelection = fallback.graphSelection;
  const edgeId = url.searchParams.get("edge");
  const nodeId = url.searchParams.get("node");
  if (input.review_kind === "pull_request") {
    const loadedNodeIds = new Set(input.graph.nodes.items.map((node) => node.id));
    if (edgeId && input.graph.edges.items.some((edge) => (
      edge.id === edgeId && loadedNodeIds.has(edge.source) && loadedNodeIds.has(edge.target)
    ))) {
      graphSelection = { type: "edge", id: edgeId };
    } else if (nodeId && input.graph.nodes.items.some((node) => node.id === nodeId)) {
      graphSelection = { type: "node", id: nodeId };
    }
  }
  return { view, selectedUnitKey, selectedEventId, graphSelection };
}

function reviewLocationUrl(base: URL, location: ReviewLocation): URL {
  const url = new URL(base.origin + base.pathname);
  url.searchParams.set("view", location.view);
  url.searchParams.set("unit", location.selectedUnitKey);
  url.searchParams.set("event", location.selectedEventId);
  if (location.graphSelection.id) {
    url.searchParams.set(location.graphSelection.type, location.graphSelection.id);
  }
  return url;
}

const reviewerFamilies = [
  "family:demo-author",
  "family:demo-reviewer",
  "family:demo-auditor",
];

function formatDate(value: string): string {
  return new Intl.DateTimeFormat("en", {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(value));
}

function displaySnapshotHash(bundle: ReviewBundle, length = 8): string {
  const hash = bundle.snapshot_identity.head_hash;
  return hash ? shortHash(hash, length) : "unavailable";
}

function StatusGlyph({ status }: { status: ReviewBundle["checks"]["items"][number]["status"] }) {
  if (status === "pass") return <CheckCircle2 aria-hidden="true" />;
  if (status === "warning") return <AlertTriangle aria-hidden="true" />;
  if (status === "fail") return <XCircle aria-hidden="true" />;
  return <Clock3 aria-hidden="true" />;
}

type CapturedPage = Readonly<{
  items: readonly unknown[];
  next_cursor: string | null;
  truncated: boolean;
}>;

function isCompletePage(page: CapturedPage): boolean {
  return !page.truncated && page.next_cursor === null;
}

function isKnownEmptyPage(page: CapturedPage): boolean {
  return page.items.length === 0 && isCompletePage(page);
}

function PageNotice({
  page,
  label,
}: {
  page: CapturedPage;
  label: string;
}) {
  if (isCompletePage(page)) return null;
  return (
    <DataState
      className="page-notice"
      state="truncated"
      title={`${label} are truncated`}
      shown={page.items.length}
      reason={page.truncated
        ? "A configured export budget truncated this collection."
        : "The bundle declares another page, but this static review surface only displays the captured page."}
      context={page.next_cursor !== null ? ["cursor available"] : undefined}
    />
  );
}

function CapabilityBanner({ bundle }: { bundle: ReviewBundle }) {
  return (
    <div className="capability-banner" role="status">
      <div className="capability-primary">
        <Info aria-hidden="true" />
        <div>
          <strong>{bundle.capability.label}</strong>
          <span>
            Approval choices stay in this browser session and are not persisted. GitHub writes and
            khive mutations are unavailable.
          </span>
        </div>
      </div>
      <div className="capability-flags" aria-label="Available capabilities">
        <span className={bundle.capability.git_reads ? "ready" : "unavailable"}>
          {bundle.capability.git_reads ? "Git reads ready" : "Git reads unavailable"}
        </span>
        <span className={bundle.capability.khive_reads ? "ready" : "simulated"}>
          {bundle.capability.khive_reads ? "Khive reads ready" : "Captured khive context"}
        </span>
        <span className={bundle.capability.wasm ? "ready" : "unavailable"}>
          {bundle.capability.wasm ? "WASM available" : "WASM unavailable"}
        </span>
      </div>
    </div>
  );
}

function Header({
  bundle,
  onImport,
  onDownload,
  commandPalette,
}: {
  bundle: ReviewBundle;
  onImport: () => void;
  onDownload: () => void;
  commandPalette: ReactNode;
}) {
  return (
    <header className="topbar">
      <div className="brand-lockup">
        <div className="brand-mark" aria-hidden="true">
          <span />
          <span />
          <span />
        </div>
        <span className="brand-name">khive</span>
        <span className="brand-product">KG Studio</span>
      </div>
      {commandPalette}
      <div className="topbar-actions">
        <button className="button quiet" type="button" onClick={onImport} aria-label="Import review bundle">
          <Upload aria-hidden="true" />
          <span className="mobile-action-label">Import bundle</span>
        </button>
        <button className="button quiet icon-only" type="button" onClick={onDownload} aria-label="Download review bundle">
          <Download aria-hidden="true" />
        </button>
        <div className="avatar avatar-atlas" title={bundle.pull_request.author}>
          A
        </div>
      </div>
    </header>
  );
}

function Sidebar({ bundle, activeView, onView }: { bundle: ReviewBundle; activeView: View; onView: (view: View) => void }) {
  const unitCount = useMemo(() => buildReviewThreadModel(bundle).units.length, [bundle]);
  const activityCount = useMemo(() => reviewActivityEvents(bundle).length, [bundle]);
  const navigation: { id: View; label: string; icon: typeof FileText; count?: number }[] = [
    { id: "changes", label: "Changes", icon: FileJson2, count: unitCount },
    { id: "graph", label: "Affected graph", icon: Network, count: bundle.graph.nodes.items.length },
    { id: "retrieval", label: "Khive context", icon: Brain },
    { id: "activity", label: "Activity", icon: Activity, count: activityCount },
  ];

  return (
    <aside className="sidebar">
      <div className="repo-switcher">
        <div className="repo-icon">
          <Database aria-hidden="true" />
        </div>
        <div>
          <span>{bundle.repository.owner}</span>
          <strong>{bundle.repository.name}</strong>
        </div>
        <ChevronDown aria-hidden="true" />
      </div>

      <nav className="side-nav" aria-label="Review navigation" data-keyboard-list>
        <span className="side-label">Review</span>
        {navigation.map((item) => {
          const Icon = item.icon;
          return (
            <button
              key={item.id}
              data-keyboard-row
              aria-keyshortcuts="J K Enter"
              className={activeView === item.id ? "active" : ""}
              type="button"
              onClick={() => onView(item.id)}
            >
              <Icon aria-hidden="true" />
              <span>{item.label}</span>
              {item.count !== undefined && <em>{item.count}</em>}
            </button>
          );
        })}
      </nav>

      <div className="branch-block">
        <span className="side-label">Branch</span>
        <div className="branch-current">
          <GitBranch aria-hidden="true" />
          <span>{bundle.repository.head_branch}</span>
        </div>
      </div>

      <div className="mini-history">
        <span className="side-label">Recent graph history</span>
        {bundle.enrichment_status.commits === "unavailable" && <span className="side-unavailable">Not captured in this bundle</span>}
        {bundle.enrichment_status.commits !== "unavailable" && bundle.commits.items.map((commit, index) => (
          <div className="mini-commit" key={commit.sha}>
            <div className="commit-rail" aria-hidden="true">
              <Circle className={commit.state} />
              {index < bundle.commits.items.length - 1 && <span />}
            </div>
            <div>
              <strong>{commit.subject}</strong>
              <span>{shortHash(commit.sha)} · {formatDate(commit.created_at)}</span>
            </div>
          </div>
        ))}
      </div>

      <div className="sidebar-footer">
        <LockKeyhole aria-hidden="true" />
        <div>
          <strong>Local-first session</strong>
          <span>No remote mutations</span>
        </div>
      </div>
    </aside>
  );
}

function PullRequestHeader({ bundle, onCopy }: { bundle: ReviewBundle; onCopy: () => void }) {
  const totalAdded = bundle.summary.entities_added + bundle.summary.edges_added;
  const totalRemoved = bundle.summary.entities_removed + bundle.summary.edges_removed;

  return (
    <section className="pr-header">
      <div className="breadcrumb">
        <span>{bundle.repository.owner}</span>
        <span>/</span>
        <strong>{bundle.repository.name}</strong>
        <span>/</span>
        <span>reviews</span>
        <span>/</span>
        <span>{bundle.pull_request.number}</span>
      </div>
      <div className="pr-heading-row">
        <div>
          <div className="pr-kicker">
            <span className="open-pill"><GitPullRequest aria-hidden="true" /> {bundle.pull_request.state}</span>
            <span>Attributed ADR-101 change-set review</span>
          </div>
          <h1>{bundle.pull_request.title}</h1>
        </div>
        <button className="button outline" type="button" onClick={onCopy}>
          <Copy aria-hidden="true" />
          Copy CLI
        </button>
      </div>
      <p className="pr-description">{bundle.pull_request.body}</p>
      <div className="pr-meta">
        <span><Bot aria-hidden="true" /> {bundle.pull_request.author}</span>
        <span><GitCommitHorizontal aria-hidden="true" /> {shortHash(bundle.repository.base_sha)} <ArrowRight /> {shortHash(bundle.repository.head_sha)}</span>
        <span className="additions">+{totalAdded}</span>
        <span className="deletions">−{totalRemoved}</span>
        <span><Clock3 aria-hidden="true" /> {formatDate(bundle.pull_request.created_at)}</span>
      </div>
    </section>
  );
}

function WorkspaceTabs({ activeView, onView, bundle }: { activeView: View; onView: (view: View) => void; bundle: ReviewBundle }) {
  const unitCount = useMemo(() => buildReviewThreadModel(bundle).units.length, [bundle]);
  const activityCount = useMemo(() => reviewActivityEvents(bundle).length, [bundle]);
  const tabs: { id: View; label: string; count?: number }[] = [
    { id: "changes", label: "Changes", count: unitCount },
    { id: "graph", label: "Graph", count: bundle.graph.nodes.items.length },
    { id: "retrieval", label: "Context" },
    { id: "activity", label: "Activity", count: activityCount },
  ];
  return (
    <div className="workspace-tabs" role="tablist" aria-label="Review views" data-keyboard-list>
      {tabs.map((tab) => (
        <button
          type="button"
          data-keyboard-row
          aria-keyshortcuts="J K Enter"
          role="tab"
          aria-selected={activeView === tab.id}
          className={activeView === tab.id ? "active" : ""}
          key={tab.id}
          onClick={() => onView(tab.id)}
        >
          {tab.label}
          {tab.count !== undefined && <span>{tab.count}</span>}
        </button>
      ))}
    </div>
  );
}

function GraphView({
  bundle,
  onImport,
  selection,
  onSelect,
}: {
  bundle: ReviewBundle;
  onImport: () => void;
  selection: GraphSelection;
  onSelect: (selection: GraphSelection) => void;
}) {

  const settledNodes = useMemo(
    () => settleGraphLayout(bundle.graph.nodes.items, bundle.graph.edges.items),
    [bundle.graph.edges.items, bundle.graph.nodes.items],
  );

  const nodeById = useMemo(
    () => new Map(settledNodes.map((node) => [node.id, node])),
    [settledNodes],
  );
  const edgeById = useMemo(() => new Map(bundle.graph.edges.items.map((edge) => [edge.id, edge])), [bundle.graph.edges.items]);

  const selectNode = (id: string) => onSelect({ type: "node", id });
  const selectEdge = (id: string) => onSelect({ type: "edge", id });

  const selected = selection.type === "node"
    ? settledNodes.find((node) => node.id === selection.id) ?? settledNodes[0]
    : undefined;
  const selectedEdge = selection.type === "edge" ? edgeById.get(selection.id) : undefined;
  const selectedEdgeSource = selectedEdge ? nodeById.get(selectedEdge.source) : undefined;
  const selectedEdgeTarget = selectedEdge ? nodeById.get(selectedEdge.target) : undefined;

  if (settledNodes.length === 0) {
    const graphKnownEmpty = isKnownEmptyPage(bundle.graph.nodes) && isKnownEmptyPage(bundle.graph.edges);
    return (
      <div className="view-stack">
        {graphKnownEmpty && <DataState
          className="empty-state"
          state="empty"
          title="No affected graph nodes in this review bundle"
          message="Affected graph nodes and their relationships belong here."
          action={{ label: "Import another review bundle", onClick: onImport }}
        />}
        <PageNotice page={bundle.graph.nodes} label="Graph nodes" />
        <PageNotice page={bundle.graph.edges} label="Graph edges" />
      </div>
    );
  }

  return (
    <div className="view-stack">
      <div className="surface-toolbar">
        <div>
          <span className="eyebrow">Bounded 2-hop context</span>
          <h2>Affected subgraph</h2>
        </div>
        <div className="graph-legend-block">
          <div className="graph-legend">
            <span><i className="added" /> Added</span>
            <span><i className="modified" /> Changed</span>
            <span><i className="context" /> Context</span>
          </div>
          <OntologyLegend
            className="graph-ontology-legend"
            presentEntityKinds={bundle.graph.nodes.items.map((node) => node.kind)}
            presentRelations={bundle.graph.edges.items.map((edge) => edge.relation)}
          />
        </div>
      </div>
      <div className="graph-stage">
        <svg className="graph-lines" viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
          <defs>
            <marker id="studio-ontology-arrow" markerHeight="6" markerWidth="6" orient="auto" refX="5" refY="3" viewBox="0 0 6 6">
              <path d="M 0 0 L 6 3 L 0 6 z" fill="context-stroke" />
            </marker>
          </defs>
          {bundle.graph.edges.items.map((edge) => {
            const source = nodeById.get(edge.source);
            const target = nodeById.get(edge.target);
            if (!source || !target) return null;
            const legend = edgeLegendFor(edge.relation);
            const direction = edgeDirectionMark(legend, source, target);
            return (
              <g key={edge.id} className={edge.state}>
                <line
                  className="ontology-edge"
                  data-edge-family={legend.family}
                  data-edge-treatment={legend.treatment}
                  data-edge-variant={legend.variant}
                  data-edge-origin="ingested"
                  markerEnd={legend.directed ? "url(#studio-ontology-arrow)" : undefined}
                  style={edgeHueStyle(legend)}
                  x1={source.x}
                  y1={source.y}
                  x2={target.x}
                  y2={target.y}
                  vectorEffect="non-scaling-stroke"
                />
                <text
                  className="ontology-edge-glyph"
                  data-edge-directed={legend.directed}
                  style={edgeHueStyle(legend)}
                  x={(source.x + target.x) / 2}
                  y={(source.y + target.y) / 2}
                >
                  {legend.glyph}
                </text>
                {direction && (
                  <text
                    className="ontology-direction-glyph"
                    style={edgeHueStyle(legend)}
                    transform={direction.transform}
                    x={direction.x}
                    y={direction.y}
                  >›</text>
                )}
              </g>
            );
          })}
        </svg>
        {settledNodes.map((node) => (
          <button
            type="button"
            className={`graph-node ${node.state} ${selection.type === "node" && node.id === selection.id ? "selected" : ""}`}
            aria-pressed={selection.type === "node" && node.id === selection.id}
            style={{ left: `${node.x}%`, top: `${node.y}%`, ...kindHueStyle(entityLegendFor(node.kind)) }}
            key={node.id}
            onClick={() => selectNode(node.id)}
          >
            <EntityKindMark className="node-kind" kind={node.kind} />
            <strong>{node.label}</strong>
          </button>
        ))}
        {bundle.graph.edges.items.map((edge) => {
          const source = nodeById.get(edge.source);
          const target = nodeById.get(edge.target);
          if (!source || !target) return null;
          return (
            <button
              type="button"
              key={`${edge.id}-label`}
              className={`edge-label ${edge.state} ${selection.type === "edge" && edge.id === selection.id ? "selected" : ""}`}
              style={{ left: `${(source.x + target.x) / 2}%`, top: `${(source.y + target.y) / 2}%` }}
              aria-pressed={selection.type === "edge" && edge.id === selection.id}
              onClick={() => selectEdge(edge.id)}
            >
              <RelationMark relation={edge.relation} /> · {edge.weight.toFixed(2)}
            </button>
          );
        })}
      </div>
      <section className="graph-edge-summary" aria-label="Affected graph relationships">
        <h3>Relationships</h3>
        {isKnownEmptyPage(bundle.graph.edges) ? (
          <DataState
            className="empty-state"
            state="empty"
            title="No affected graph relationships in this review bundle"
            message="Relationships between affected graph nodes belong here."
            action={{ label: "Import another review bundle", onClick: onImport }}
          />
        ) : <ul>
          {bundle.graph.edges.items.map((edge) => {
            const source = nodeById.get(edge.source);
            const target = nodeById.get(edge.target);
            if (!source || !target) return null;
            return (
              <li key={`${edge.id}-summary`}>
                <button
                  type="button"
                  data-keyboard-row
                  aria-keyshortcuts="J K Enter"
                  className={selection.type === "edge" && edge.id === selection.id ? "selected" : ""}
                  aria-pressed={selection.type === "edge" && edge.id === selection.id}
                  onClick={() => selectEdge(edge.id)}
                >
                  <strong>{source.label}</strong>
                  <span><RelationMark relation={edge.relation} /><span className="visually-hidden">{edge.relation}</span> · {edge.weight.toFixed(2)}</span>
                  <strong>{target.label}</strong>
                  <em>{edge.state}</em>
                </button>
              </li>
            );
          })}
        </ul>}
      </section>
      {selected && (
        <div className="node-inspector" style={kindHueStyle(entityLegendFor(selected.kind))}>
          <div className={`node-state-dot ${selected.state}`} />
          <div>
            <EntityKindMark className="node-inspector-kind" kind={selected.kind} showLabel={false} />
            <span>{entityLegendFor(selected.kind).label} · {selected.state}</span>
            <strong>{selected.label}</strong>
            <p>{selected.description}</p>
          </div>
          <code>{shortHash(selected.id)}</code>
        </div>
      )}
      {selectedEdge && selectedEdgeSource && selectedEdgeTarget && (
        <div className="node-inspector edge-inspector" style={edgeHueStyle(edgeLegendFor(selectedEdge.relation))}>
          <div className={`node-state-dot ${selectedEdge.state}`} />
          <div>
            <RelationMark className="node-inspector-kind" relation={selectedEdge.relation} showLabel={false} />
            <span>{edgeLegendFor(selectedEdge.relation).label} · {selectedEdge.state}</span>
            <strong>{selectedEdgeSource.label} → {selectedEdgeTarget.label}</strong>
            <p>Weight {selectedEdge.weight.toFixed(2)}</p>
          </div>
          <code>{shortHash(selectedEdge.id)}</code>
        </div>
      )}
      <PageNotice page={bundle.graph.nodes} label="Graph nodes" />
      <PageNotice page={bundle.graph.edges} label="Graph edges" />
    </div>
  );
}

function RetrievalView({ bundle, onImport }: { bundle: ReviewBundle; onImport: () => void }) {
  const [mode, setMode] = useState<"search" | "recall" | "traverse">("search");
  const activePage = mode === "traverse" ? bundle.retrieval.traversal : bundle.retrieval[mode];
  return (
    <div className="view-stack retrieval-view">
      <div className="surface-toolbar">
        <div><span className="eyebrow">{bundle.enrichment_status.retrieval === "live" ? "Live khive results" : "Simulated from captured khive results"}</span><h2>Review context</h2></div>
        <div className="segmented-control">
          {(["search", "recall", "traverse"] as const).map((item) => (
            <button className={mode === item ? "active" : ""} type="button" key={item} onClick={() => setMode(item)}>
              {item === "search" ? <Search /> : item === "recall" ? <Brain /> : <GitFork />}
              {item}
            </button>
          ))}
        </div>
      </div>
      <div className="query-box">
        <Sparkles aria-hidden="true" />
        <span>assertion provenance review auditability</span>
        <kbd>{mode}</kbd>
      </div>
      {isKnownEmptyPage(activePage) && (
        <DataState
          className="empty-state"
          state="empty"
          title={`No ${mode} results in this review bundle`}
          message="Captured khive retrieval context belongs here."
          action={{ label: "Import another review bundle", onClick: onImport }}
        />
      )}
      {activePage.items.length > 0 && mode === "search" && (
        <div className="retrieval-results">
          {bundle.retrieval.search.items.map((result, index) => (
            <article key={result.id}><span className="result-rank">{index + 1}</span><div><strong>{result.title}</strong><span><OntologyKindMark kind={result.kind} /> · score {result.score}</span><p>{result.snippet}</p></div><code>{result.id}</code></article>
          ))}
        </div>
      )}
      {activePage.items.length > 0 && mode === "recall" && (
        <div className="retrieval-results">
          {bundle.retrieval.recall.items.map((result, index) => (
            <article key={result.id}><span className="result-rank memory">{index + 1}</span><div><strong>{result.memory_type} memory</strong><span>decay-aware score {result.score.toFixed(3)}</span><p>{result.content}</p></div><code>{result.id}</code></article>
          ))}
        </div>
      )}
      {activePage.items.length > 0 && mode === "traverse" && (
        <div className="traversal-list">
          {bundle.retrieval.traversal.items.map((node, index) => (
            <div className={`traversal-row depth-${node.depth}`} key={`${node.id}-${index}`}>
              <span className="traversal-line" aria-hidden="true" />
              <span className="traversal-node"><EntityKindMark kind={node.kind} showLabel={false} /></span>
              <div><strong>{node.name}</strong><span><EntityKindMark kind={node.kind} />{node.via ? <> · <RelationMark relation={node.via} /></> : " · root"}</span></div>
              <code>{node.id}</code>
            </div>
          ))}
        </div>
      )}
      <PageNotice page={activePage} label={`${mode} results`} />
    </div>
  );
}

function ReviewRail({
  bundle,
  reviewerFamily,
  onReviewerFamily,
  decision,
  onDecision,
}: {
  bundle: ReviewBundle;
  reviewerFamily: string;
  onReviewerFamily: (value: string) => void;
  decision: ReviewDecision;
  onDecision: (decision: Exclude<ReviewDecision, "pending">) => void;
}) {
  const gate = canApproveReview(bundle, reviewerFamily);
  const passed = bundle.checks.items.filter((check) => check.status === "pass").length;
  const warnings = bundle.checks.items.filter((check) => check.status === "warning").length;

  return (
    <aside className="review-rail">
      <section className="rail-card review-gate-card">
        <div className="rail-heading"><div><span className="eyebrow">Review gate</span><h3>Independent approval</h3></div><ShieldCheck /></div>
        <div className={`gate-status ${gate.allowed ? "allowed" : "blocked"}`}>
          {gate.allowed ? <CheckCircle2 /> : <AlertTriangle />}
          <div><strong>{gate.allowed ? "Ready to review" : "Approval blocked"}</strong><span>{gate.reason}</span></div>
        </div>
        <label className="select-label">
          <span>Reviewer model family</span>
          <div><select value={reviewerFamily} onChange={(event) => onReviewerFamily(event.target.value)}>{reviewerFamilies.map((family) => <option key={family}>{family}</option>)}</select><ChevronDown /></div>
        </label>
        <div className="decision-actions">
          <button className="button danger-outline" type="button" onClick={() => onDecision("changes_requested")}>Request changes</button>
          <button className="button approve" type="button" onClick={() => onDecision("approved")}>
            <Check /> Approve locally
          </button>
        </div>
        {decision !== "pending" && (
          <div className={`local-decision ${decision}`}><Circle /> Local decision: {decision === "approved" ? "approved" : "changes requested"}</div>
        )}
      </section>

      <section className="rail-card">
        <div className="rail-heading"><div><span className="eyebrow">Change-set</span><h3>Risk routing</h3></div><FileJson2 /></div>
        <div className="tier-meter"><span style={{ width: `${(bundle.summary.tier_1 / Math.max(1, bundle.change_set.operations.length)) * 100}%` }} /><i /></div>
        <div className="tier-counts">
          <div><strong>{bundle.summary.tier_1}</strong><span>Tier 1 additive</span></div>
          <div><strong>{bundle.summary.tier_2}</strong><span>Tier 2 reviewed</span></div>
        </div>
        <div className="producer-row"><div className="avatar avatar-atlas">A</div><div><span>Produced by</span><strong>{bundle.change_set.envelope.producer}</strong><small>{bundle.change_set.envelope.producer_model_family}</small></div></div>
      </section>

      <section className="rail-card">
        <div className="rail-heading"><div><span className="eyebrow">Checks</span><h3>{passed} passed · {warnings} warning</h3></div><CheckCircle2 /></div>
        <div className="checks-list compact">
          {bundle.checks.items.map((check) => (
            <div className={check.status} key={check.id}><StatusGlyph status={check.status} /><span>{check.label}</span></div>
          ))}
        </div>
      </section>

      <section className="rail-card refs-card">
        <div><GitBranch /><span>Base</span><code>{shortHash(bundle.repository.base_sha)}</code></div>
        <div><GitCommitHorizontal /><span>Head</span><code>{shortHash(bundle.repository.head_sha)}</code></div>
        <div><Box /><span>{bundle.snapshot_identity.hash_status === "fixture" ? "Fixture KG" : "KG state"}</span><code>{displaySnapshotHash(bundle)}</code></div>
      </section>
    </aside>
  );
}

function ViewSurface({
  activeView,
  bundle,
  query,
  onQuery,
  onImport,
  selectedUnitKey,
  onSelectUnit,
  selectedEventId,
  onSelectEvent,
  graphSelection,
  onSelectGraph,
  activityDraft,
  onActivityDraft,
  annotations,
  onAddLocalNote,
}: {
  activeView: View;
  bundle: ReviewBundle;
  query: string;
  onQuery: (value: string) => void;
  onImport: () => void;
  selectedUnitKey: string;
  onSelectUnit: (key: string) => void;
  selectedEventId: string;
  onSelectEvent: (id: string) => void;
  graphSelection: GraphSelection;
  onSelectGraph: (selection: GraphSelection) => void;
  activityDraft: string;
  onActivityDraft: (value: string) => void;
  annotations: readonly ReviewAnnotation[];
  onAddLocalNote: () => void;
}) {
  const unavailable =
    (activeView === "graph" && bundle.enrichment_status.affected_graph === "unavailable") ||
    (activeView === "retrieval" && bundle.enrichment_status.retrieval === "unavailable");
  if (unavailable) {
    return (
      <DataState
        className="unavailable-surface"
        state="unavailable"
        title={`${viewLabels[activeView]} unavailable`}
        message="This review bundle did not claim or invent that enrichment."
      />
    );
  }
  if (activeView === "graph") return <GraphView bundle={bundle} onImport={onImport} selection={graphSelection} onSelect={onSelectGraph} />;
  if (activeView === "retrieval") return <RetrievalView bundle={bundle} onImport={onImport} />;
  if (activeView === "activity") return <ReviewActivityTimeline input={bundle} selectedEventId={selectedEventId} onSelectEvent={onSelectEvent} />;
  return <ReviewThreadSurface input={bundle} selectedUnitKey={selectedUnitKey} onSelectUnit={onSelectUnit} annotations={annotations} query={query} onQuery={onQuery} onImport={onImport} draft={activityDraft} onDraft={onActivityDraft} onAddLocalNote={onAddLocalNote} />;
}

function CoreReviewStudio({
  report,
  onImport,
  onDownload,
  onUseDemo,
  activeView,
  onView,
  selectedUnitKey,
  onSelectUnit,
  selectedEventId,
  onSelectEvent,
  annotations,
  draft,
  onDraft,
  onAddLocalNote,
}: {
  report: ReviewReport;
  onImport: () => void;
  onDownload: () => void;
  onUseDemo: () => void;
  activeView: View;
  onView: (view: View) => void;
  selectedUnitKey: string;
  onSelectUnit: (key: string) => void;
  selectedEventId: string;
  onSelectEvent: (id: string) => void;
  annotations: readonly ReviewAnnotation[];
  draft: string;
  onDraft: (value: string) => void;
  onAddLocalNote: () => void;
}) {
  return (
    <div className="app-shell core-report-shell" onKeyDown={handleKeyboardRows}>
      <header className="topbar">
        <div className="brand-lockup">
          <div className="brand-mark" aria-hidden="true"><span /><span /><span /></div>
          <span className="brand-name">khive</span>
          <span className="brand-product">KG Studio</span>
        </div>
        <RepositoryCommandPalette
          surface="review"
          triggerClassName="global-search"
          views={[{ id: "changes", label: "Changes" }, { id: "activity", label: "Activity" }]}
          activeReviewView={activeView}
          onSelectReviewView={(view) => onView(view as View)}
          hasUnsavedReviewState
          onDownloadReview={onDownload}
          downloadSubject="report"
        />
        <div className="topbar-actions">
          <button className="button quiet" type="button" onClick={onUseDemo} aria-label="Use demo review bundle"><Database aria-hidden="true" /><span className="mobile-action-label">Use demo</span></button>
          <button className="button quiet" type="button" onClick={onImport} aria-label="Import review report"><Upload aria-hidden="true" /><span className="mobile-action-label">Import</span></button>
          <button className="button quiet icon-only" type="button" onClick={onDownload} aria-label="Download review report"><Download /></button>
        </div>
      </header>

      <main className="core-workspace">
        <div className="capability-banner" role="status">
          <div className="capability-primary">
            <Info aria-hidden="true" />
            <div><strong>Imported CLI report · no writes</strong><span>This is the minimal shared review core; no Git or GitHub metadata has been invented.</span></div>
          </div>
          <div className="capability-flags"><span className="ready">Strict ADR-101 parse</span><span className={report.capability.wasm ? "ready" : "unavailable"}>{report.capability.wasm ? "WASM available" : "WASM unavailable"}</span></div>
        </div>

        <section className="pr-header core-report-header">
          <div className="breadcrumb"><span>local change-set</span><span>/</span><strong>{report.change_set.envelope.batch_id ?? "derived batch identity"}</strong></div>
          <div className="pr-kicker"><span className="open-pill"><ShieldCheck /> Review report</span><span>{report.review_gate.status.replaceAll("_", " ")}</span></div>
          <h1>Attributed change-set review</h1>
          <p className="pr-description">{report.review_gate.reason}</p>
          <div className="pr-meta">
            <span><Bot /> {report.change_set.envelope.producer}</span>
            <span><FileJson2 /> {report.change_set.operations.length} ordered operations</span>
            <span><Clock3 /> staged {report.change_set.envelope.staged_at} µs</span>
          </div>
        </section>

        <div className="workspace-tabs" role="tablist" aria-label="Review views" data-keyboard-list>
          {(["changes", "activity"] as const).map((view) => (
            <button key={view} type="button" role="tab" data-keyboard-row aria-keyshortcuts="J K Enter"
              aria-selected={activeView === view} className={activeView === view ? "active" : ""}
              onClick={() => onView(view)}>{viewLabels[view]}</button>
          ))}
        </div>

        <div className="review-layout core-review-layout">
          <section className="review-surface" aria-label={viewLabels[activeView]} data-keyboard-scope tabIndex={-1}>
            {activeView === "activity" ? (
              <ReviewActivityTimeline input={report} selectedEventId={selectedEventId} onSelectEvent={onSelectEvent} />
            ) : (
              <ReviewThreadSurface input={report} selectedUnitKey={selectedUnitKey} onSelectUnit={onSelectUnit}
                annotations={annotations} draft={draft} onDraft={onDraft} onAddLocalNote={onAddLocalNote} />
            )}
          </section>

          <aside className="review-rail">
            <section className="rail-card review-gate-card">
              <div className="rail-heading"><div><span className="eyebrow">Review gate</span><h3>{report.review_gate.approval_ready ? "Approval-ready" : "Not approval-ready"}</h3></div><ShieldCheck /></div>
              <div className={`gate-status ${report.review_gate.approval_ready ? "allowed" : "blocked"}`}>
                {report.review_gate.approval_ready ? <CheckCircle2 /> : <AlertTriangle />}
                <div><strong>{report.review_gate.status.replaceAll("_", " ")}</strong><span>{report.review_gate.reason}</span></div>
              </div>
              <div className="core-family-pair"><span>Producer family</span><code>{report.review_gate.producer_model_family}</code><span>Reviewer family</span><code>{report.review_gate.reviewer_model_family ?? "not supplied"}</code></div>
            </section>

            <section className="rail-card">
              <div className="rail-heading"><div><span className="eyebrow">Risk routing</span><h3>{report.tier_summary.highest_tier.replace("_", " ").toUpperCase()}</h3></div><FileJson2 /></div>
              <div className="tier-counts"><div><strong>{report.tier_summary.tier_1}</strong><span>Tier 1</span></div><div><strong>{report.tier_summary.tier_2}</strong><span>Tier 2</span></div></div>
            </section>

            <section className="rail-card">
              <div className="rail-heading"><div><span className="eyebrow">Partial-view validation</span><h3>{report.validation.passed ? "Passed" : "Failed"}</h3></div>{report.validation.passed ? <CheckCircle2 /> : <XCircle />}</div>
              <div className="core-validation-counts"><span>{report.validation.errors} errors</span><span>{report.validation.warnings} warnings</span><span>{report.validation.info} info</span></div>
              {report.findings.length > 0 && <div className="checks-list compact">{report.findings.map((finding, index) => <div className={finding.severity.toLowerCase()} key={`${finding.rule_id}-${index}`}><StatusGlyph status={finding.severity.toLowerCase() === "error" ? "fail" : finding.severity.toLowerCase() === "warning" ? "warning" : "pass"} /><span>{finding.rule_id}: {finding.message}</span></div>)}</div>}
            </section>
          </aside>
        </div>
      </main>
    </div>
  );
}

export function Studio({ initialBundle }: { initialBundle: ReviewBundle }) {
  const [bundle, setBundle] = useState(initialBundle);
  const [coreReport, setCoreReport] = useState<ReviewReport | null>(null);
  const [reviewLocation, setReviewLocation] = useState(() => defaultReviewLocation(initialBundle));
  const reviewLocationRef = useRef(reviewLocation);
  const activeView = reviewLocation.view;
  const [query, setQuery] = useState("");
  const [reviewerFamily, setReviewerFamily] = useState(bundle.change_set.envelope.producer_model_family);
  const [decision, setDecision] = useState<ReviewDecision>("pending");
  const [activityDraft, setActivityDraft] = useState("");
  const [localNotes, setLocalNotes] = useState<ReviewAnnotation[]>([]);
  const [coreDraft, setCoreDraft] = useState("");
  const [coreNotes, setCoreNotes] = useState<ReviewAnnotation[]>([]);
  const [toast, setToast] = useState<Toast>(null);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);

  useEffect(() => {
    function restoreLocation() {
      const currentUrl = new URL(window.location.href);
      const restored = reviewLocationFromUrl(currentUrl, coreReport ?? bundle);
      const canonical = reviewLocationUrl(currentUrl, restored);
      if (canonical.href !== currentUrl.href) {
        window.history.replaceState(null, "", `${canonical.pathname}${canonical.search}`);
      }
      reviewLocationRef.current = restored;
      setReviewLocation(restored);
    }

    restoreLocation();
    window.addEventListener("popstate", restoreLocation);
    return () => window.removeEventListener("popstate", restoreLocation);
  }, [bundle, coreReport]);

  function pushReviewLocation(next: ReviewLocation) {
    const url = reviewLocationUrl(new URL(window.location.href), next);
    if (url.href !== window.location.href) {
      window.history.pushState(null, "", `${url.pathname}${url.search}`);
    }
    reviewLocationRef.current = next;
    setReviewLocation(next);
  }

  function selectView(view: View) {
    pushReviewLocation({ ...reviewLocationRef.current, view });
  }

  function selectUnit(key: string) {
    const current = reviewLocationRef.current;
    pushReviewLocation({
      ...current,
      selectedUnitKey: key,
    });
  }

  function selectEvent(id: string) {
    pushReviewLocation({ ...reviewLocationRef.current, selectedEventId: id });
  }

  function selectGraph(selection: GraphSelection) {
    pushReviewLocation({ ...reviewLocationRef.current, graphSelection: selection });
  }

  function showToast(next: Toast) {
    setToast(next);
    window.setTimeout(() => setToast(null), 3600);
  }

  async function importBundle(file: File | undefined) {
    if (!file) return;
    try {
      if (file.size > REVIEW_IMPORT_MAX_BYTES) {
        throw new Error("Bundle exceeds the 2 MiB local import limit.");
      }
      const parsed = parseReviewInput(JSON.parse(await file.text()));
      if (isReviewReport(parsed)) {
        setCoreReport(parsed);
        setCoreDraft("");
        setCoreNotes([]);
        showToast({ tone: "success", message: "Loaded a read-only khive CLI review report." });
        return;
      }
      setBundle({
        ...parsed,
        capability: {
          ...parsed.capability,
          source: "import",
          label: "Imported bundle · no writes",
          no_writes: true,
          git_reads: false,
          khive_reads: false,
          github_writes: false,
          wasm: false,
          persistence: false,
        },
      });
      setReviewerFamily(parsed.change_set.envelope.producer_model_family);
      setDecision("pending");
      setActivityDraft("");
      setLocalNotes([]);
      setCoreReport(null);
      showToast({ tone: "success", message: `Loaded review bundle for ${parsed.repository.owner}/${parsed.repository.name}.` });
    } catch (error) {
      showToast({ tone: "warning", message: error instanceof Error ? `Bundle rejected: ${error.message}` : "Bundle rejected." });
    } finally {
      if (fileInput.current) fileInput.current.value = "";
    }
  }

  function downloadBundle() {
    const value = coreReport ?? bundle;
    const blob = new Blob([JSON.stringify(value, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = coreReport
      ? `${coreReport.change_set.envelope.batch_id ?? "changeset"}-review.json`
      : `${bundle.repository.name}-review-${bundle.pull_request.number}.json`;
    anchor.click();
    URL.revokeObjectURL(url);
  }

  async function copyCli() {
    const command = "khive kg review changes.ndjson --rules rules.toml --format json";
    await navigator.clipboard.writeText(command);
    showToast({ tone: "neutral", message: "Copied the headless review command." });
  }

  function handleDecision(next: Exclude<ReviewDecision, "pending">) {
    if (next === "approved") {
      const gate = canApproveReview(bundle, reviewerFamily);
      if (!gate.allowed) {
        showToast({ tone: "warning", message: gate.reason });
        return;
      }
    }
    setDecision(next);
    showToast({
      tone: next === "approved" ? "success" : "warning",
      message: `${next === "approved" ? "Approval" : "Change request"} recorded in this browser session only.`,
    });
  }

  function addNote(
    draft: string,
    setNotes: (update: (current: ReviewAnnotation[]) => ReviewAnnotation[]) => void,
    clearDraft: () => void,
  ) {
    const note = draft.trim();
    if (!note) return;
    setNotes((current) => [...current, {
      id: `local-${Date.now()}-${current.length}`,
      unitKey: reviewLocationRef.current.selectedUnitKey,
      actor: "you",
      body: note,
      createdAt: new Date().toISOString(),
    }]);
    clearDraft();
  }
  const addLocalNote = () => addNote(activityDraft, setLocalNotes, () => setActivityDraft(""));
  const addCoreNote = () => addNote(coreDraft, setCoreNotes, () => setCoreDraft(""));

  const hasUnsavedReviewState = coreReport !== null || bundle !== initialBundle ||
    decision !== "pending" ||
    reviewerFamily !== bundle.change_set.envelope.producer_model_family ||
    activityDraft.trim().length > 0 || localNotes.length > 0 ||
    coreDraft.trim().length > 0 || coreNotes.length > 0;

  const filePicker = (
    <input
      ref={fileInput}
      className="visually-hidden"
      type="file"
      accept="application/json,.json"
      onChange={(event) => void importBundle(event.target.files?.[0])}
    />
  );

  if (coreReport) {
    return (
      <>
        {filePicker}
        <CoreReviewStudio
          report={coreReport}
          onImport={() => fileInput.current?.click()}
          onDownload={downloadBundle}
          onUseDemo={() => setCoreReport(null)}
          activeView={activeView}
          onView={selectView}
          selectedUnitKey={reviewLocation.selectedUnitKey}
          onSelectUnit={selectUnit}
          selectedEventId={reviewLocation.selectedEventId}
          onSelectEvent={selectEvent}
          annotations={coreNotes}
          draft={coreDraft}
          onDraft={setCoreDraft}
          onAddLocalNote={addCoreNote}
        />
        {toast && <div className={`toast ${toast.tone}`} role="status"><CheckCircle2 /><span>{toast.message}</span><button type="button" onClick={() => setToast(null)} aria-label="Dismiss"><X /></button></div>}
      </>
    );
  }

  return (
    <div className="app-shell" onKeyDown={handleKeyboardRows}>
      <Header
        bundle={bundle}
        onImport={() => fileInput.current?.click()}
        onDownload={downloadBundle}
        commandPalette={
          <RepositoryCommandPalette
            surface="review"
            triggerClassName="global-search"
            views={Object.entries(viewLabels).map(([id, label]) => ({ id, label }))}
            activeReviewView={activeView}
            hasUnsavedReviewState={hasUnsavedReviewState}
            onSelectReviewView={(view) => {
              selectView(view as View);
              queueMicrotask(() => document.querySelector<HTMLElement>(".review-surface")?.focus());
            }}
            onCopyCli={copyCli}
            onDownloadReview={downloadBundle}
          />
        }
      />
      {filePicker}
      <button className="mobile-menu" type="button" onClick={() => setSidebarOpen((open) => !open)} aria-label="Toggle navigation"><Menu /></button>
      <div className="app-body">
        <div className={`sidebar-wrap ${sidebarOpen ? "open" : ""}`} onClick={() => setSidebarOpen(false)}>
          <Sidebar bundle={bundle} activeView={activeView} onView={(view) => { selectView(view); setSidebarOpen(false); }} />
        </div>
        <main className="workspace">
          <CapabilityBanner bundle={bundle} />
          <PullRequestHeader bundle={bundle} onCopy={() => void copyCli()} />
          <WorkspaceTabs activeView={activeView} onView={selectView} bundle={bundle} />
          <div className="review-layout">
            <section className="review-surface" aria-label={viewLabels[activeView]} data-keyboard-scope tabIndex={-1}>
              <ViewSurface
                key={`${bundle.repository.owner}/${bundle.repository.name}#${bundle.pull_request.number}@${bundle.pull_request.head_sha}`}
                activeView={activeView}
                bundle={bundle}
                query={query}
                onQuery={setQuery}
                onImport={() => fileInput.current?.click()}
                selectedUnitKey={reviewLocation.selectedUnitKey}
                onSelectUnit={selectUnit}
                selectedEventId={reviewLocation.selectedEventId}
                onSelectEvent={selectEvent}
                graphSelection={reviewLocation.graphSelection}
                onSelectGraph={selectGraph}
                activityDraft={activityDraft}
                onActivityDraft={setActivityDraft}
                annotations={localNotes}
                onAddLocalNote={addLocalNote}
              />
            </section>
            <ReviewRail
              bundle={bundle}
              reviewerFamily={reviewerFamily}
              onReviewerFamily={(value) => {
                setReviewerFamily(value);
                setDecision("pending");
              }}
              decision={decision}
              onDecision={handleDecision}
            />
          </div>
        </main>
      </div>
      {toast && (
        <div className={`toast ${toast.tone}`} role="status">
          {toast.tone === "success" ? <CheckCircle2 /> : toast.tone === "warning" ? <AlertTriangle /> : <Info />}
          <span>{toast.message}</span>
          <button type="button" onClick={() => setToast(null)} aria-label="Dismiss"><X /></button>
        </div>
      )}
    </div>
  );
}
