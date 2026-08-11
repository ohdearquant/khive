"use client";

import {
  Activity,
  AlertTriangle,
  ArrowRight,
  BookOpen,
  Bot,
  Box,
  Brain,
  Check,
  CheckCircle2,
  ChevronDown,
  Circle,
  Clock3,
  Code2,
  Copy,
  Database,
  Download,
  ExternalLink,
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
} from "lucide-react";
import { useMemo, useRef, useState } from "react";

import {
  edgeDirectionMark,
  edgeHueStyle,
  EntityKindMark,
  kindHueStyle,
  NoteKindMark,
  OntologyKindMark,
  OntologyLegend,
  RelationMark,
} from "@/components/ontology-mark";
import { edgeLegendFor, entityLegendFor } from "@/lib/ontology-legend";
import {
  isReviewReport,
  parseReviewInput,
  REVIEW_IMPORT_MAX_BYTES,
  type ReviewBundle,
  type ReviewChange,
  type ReviewReport,
} from "@/lib/review-bundle";
import {
  canApproveReview,
  groupChanges,
  matchesReviewQuery,
  shortHash,
  type ReviewDecision,
} from "@/lib/review-utils";

type View = "changes" | "graph" | "checks" | "provenance" | "retrieval" | "activity";
type Toast = { tone: "success" | "warning" | "neutral"; message: string } | null;

const viewLabels: Record<View, string> = {
  changes: "Changes",
  graph: "Affected graph",
  checks: "Checks",
  provenance: "Provenance",
  retrieval: "Khive context",
  activity: "Activity",
};

const reviewerFamilies = [
  "family:atlas-frontier",
  "family:independent-reasoner",
  "family:human-curator",
];

function formatDate(value: string): string {
  return new Intl.DateTimeFormat("en", {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(value));
}

function formatValue(value: unknown): string {
  if (typeof value === "string") return value;
  if (value === undefined) return "—";
  return JSON.stringify(value, null, 2);
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

function TierPill({ tier }: { tier: "tier_1" | "tier_2" }) {
  return <span className={`tier-pill ${tier}`}>{tier === "tier_1" ? "T1 fast path" : "T2 review"}</span>;
}

function PageNotice({ page, label }: { page: { next_cursor: string | null; truncated: boolean }; label: string }) {
  if (!page.next_cursor && !page.truncated) return null;
  return (
    <div className="page-notice" role="status">
      <Info aria-hidden="true" />
      <span>{label} are bounded to this page.{page.truncated ? " A configured budget truncated collection." : " More results are available."}</span>
      {page.next_cursor && <code>cursor available</code>}
    </div>
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
}: {
  bundle: ReviewBundle;
  onImport: () => void;
  onDownload: () => void;
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
      <div className="global-search" aria-label="Global search preview">
        <Search aria-hidden="true" />
        <span>Search this graph</span>
        <kbd>⌘ K</kbd>
      </div>
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
  const navigation: { id: View; label: string; icon: typeof FileText; count?: number }[] = [
    { id: "changes", label: "Changes", icon: FileJson2, count: bundle.changes.items.length },
    { id: "graph", label: "Affected graph", icon: Network, count: bundle.graph.nodes.items.length },
    { id: "checks", label: "Checks", icon: ShieldCheck, count: bundle.checks.items.length },
    { id: "provenance", label: "Provenance", icon: BookOpen, count: bundle.evidence.items.length },
    { id: "retrieval", label: "Khive context", icon: Brain },
    { id: "activity", label: "Activity", icon: Activity, count: bundle.activity.items.length },
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

      <nav className="side-nav" aria-label="Review navigation">
        <span className="side-label">Review</span>
        {navigation.map((item) => {
          const Icon = item.icon;
          return (
            <button
              key={item.id}
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
  const tabs: { id: View; label: string; count?: number }[] = [
    { id: "changes", label: "Changes", count: bundle.changes.items.length },
    { id: "graph", label: "Graph", count: bundle.graph.nodes.items.length },
    { id: "checks", label: "Checks", count: bundle.checks.items.length },
    { id: "provenance", label: "Evidence", count: bundle.evidence.items.length },
    { id: "retrieval", label: "Context" },
    { id: "activity", label: "Conversation", count: bundle.activity.items.length },
  ];
  return (
    <div className="workspace-tabs" role="tablist" aria-label="Review views">
      {tabs.map((tab) => (
        <button
          type="button"
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

function FieldDiff({ field }: { field: ReviewChange["fields"][number] }) {
  const changed = field.before !== undefined && field.after !== undefined;
  return (
    <div className="field-diff">
      <span className="field-name">{field.path}</span>
      <div className="field-values">
        {field.before !== undefined && (
          <pre className="before"><span>−</span>{formatValue(field.before)}</pre>
        )}
        {field.after !== undefined && (
          <pre className="after"><span>{changed ? "+" : "+"}</span>{formatValue(field.after)}</pre>
        )}
      </div>
    </div>
  );
}

function ChangeOntologyMark({ change }: { change: ReviewChange }) {
  const valueAt = (path: string) => {
    const field = change.fields.find((candidate) => candidate.path === path);
    return field?.after ?? field?.before;
  };
  if (change.substrate === "entity") {
    const value = valueAt("entity_kind") ?? valueAt("kind");
    const kind = typeof value === "string" ? value : "entity";
    return <EntityKindMark className="substrate-chip" kind={kind} />;
  }
  if (change.substrate === "note") {
    const value = valueAt("note_kind") ?? valueAt("kind");
    const kind = typeof value === "string" ? value : "note";
    return <NoteKindMark className="substrate-chip" kind={kind} />;
  }
  const value = valueAt("relation");
  const relation = typeof value === "string" ? value : "edge";
  return <RelationMark className="substrate-chip edge" relation={relation} />;
}

function ChangeCard({ change, selected, onSelect }: { change: ReviewChange; selected: boolean; onSelect: () => void }) {
  return (
    <article className={`change-card ${change.change} ${selected ? "selected" : ""}`}>
      <button className="change-summary" type="button" onClick={onSelect} aria-expanded={selected}>
        <span className="change-sign" aria-hidden="true">
          {change.change === "added" ? "+" : change.change === "removed" ? "−" : "~"}
        </span>
        <div className="change-title">
          <strong>{change.title}</strong>
          <span>{change.subtitle}</span>
        </div>
        <ChangeOntologyMark change={change} />
        <TierPill tier={change.tier} />
        <ChevronDown className={selected ? "rotated" : ""} aria-hidden="true" />
      </button>
      {selected && (
        <div className="change-detail">
          <div className="record-id"><Code2 aria-hidden="true" /> {change.id}</div>
          {change.fields.map((field) => <FieldDiff key={field.path} field={field} />)}
          <div className="evidence-links">
            <BookOpen aria-hidden="true" />
            {change.evidence_ids.length} evidence anchor{change.evidence_ids.length === 1 ? "" : "s"} travel with this change
          </div>
        </div>
      )}
    </article>
  );
}

function ChangesView({ bundle, query, onQuery }: { bundle: ReviewBundle; query: string; onQuery: (value: string) => void }) {
  const filtered = bundle.changes.items.filter((change) => matchesReviewQuery(change, query));
  const grouped = groupChanges(filtered);
  const [selectedId, setSelectedId] = useState(bundle.changes.items.at(0)?.id ?? "");

  return (
    <div className="view-stack">
      <div className="surface-toolbar">
        <div>
          <span className="eyebrow">Semantic diff</span>
          <h2>{filtered.length} graph changes</h2>
        </div>
        <label className="filter-input">
          <Search aria-hidden="true" />
          <input value={query} onChange={(event) => onQuery(event.target.value)} placeholder="Filter entities, edges, tiers…" />
          {query && <button type="button" onClick={() => onQuery("")} aria-label="Clear filter"><X /></button>}
        </label>
      </div>
      <div className="diff-legend">
        <span><i className="added" /> {grouped.added.length} added</span>
        <span><i className="modified" /> {grouped.modified.length} modified</span>
        <span><i className="removed" /> {grouped.removed.length} removed</span>
        <span className="content-hash"><Box aria-hidden="true" /> {bundle.snapshot_identity.hash_status === "fixture" ? "Fixture KG" : "KG"} {displaySnapshotHash(bundle, 10)}</span>
      </div>
      <div className="change-list">
        {filtered.map((change) => (
          <ChangeCard
            key={change.id}
            change={change}
            selected={selectedId === change.id}
            onSelect={() => setSelectedId((current) => current === change.id ? "" : change.id)}
          />
        ))}
        {filtered.length === 0 && (
          <div className="empty-state"><Search aria-hidden="true" /><strong>No changes match “{query}”</strong><span>Try a relation, entity kind, or tier.</span></div>
        )}
      </div>
      <PageNotice page={bundle.changes} label="Graph changes" />
    </div>
  );
}

type GraphSelection = { type: "node" | "edge"; id: string };

function GraphView({ bundle }: { bundle: ReviewBundle }) {
  const [selection, setSelection] = useState<GraphSelection>({
    type: "node",
    id: bundle.graph.nodes.items[0]?.id ?? "",
  });

  const nodeById = useMemo(() => new Map(bundle.graph.nodes.items.map((node) => [node.id, node])), [bundle.graph.nodes.items]);
  const edgeById = useMemo(() => new Map(bundle.graph.edges.items.map((edge) => [edge.id, edge])), [bundle.graph.edges.items]);

  const selectNode = (id: string) => setSelection({ type: "node", id });
  const selectEdge = (id: string) => setSelection({ type: "edge", id });

  const selected = selection.type === "node"
    ? bundle.graph.nodes.items.find((node) => node.id === selection.id) ?? bundle.graph.nodes.items[0]
    : undefined;
  const selectedEdge = selection.type === "edge" ? edgeById.get(selection.id) : undefined;
  const selectedEdgeSource = selectedEdge ? nodeById.get(selectedEdge.source) : undefined;
  const selectedEdgeTarget = selectedEdge ? nodeById.get(selectedEdge.target) : undefined;

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
        {bundle.graph.nodes.items.map((node) => (
          <button
            type="button"
            className={`graph-node ${node.state} ${selection.type === "node" && node.id === selection.id ? "selected" : ""}`}
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
        <ul>
          {bundle.graph.edges.items.map((edge) => {
            const source = nodeById.get(edge.source);
            const target = nodeById.get(edge.target);
            if (!source || !target) return null;
            return (
              <li key={`${edge.id}-summary`}>
                <button
                  type="button"
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
        </ul>
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

function ChecksView({ bundle }: { bundle: ReviewBundle }) {
  const failed = bundle.checks.items.filter((check) => check.status === "fail").length;
  return (
    <div className="view-stack">
      <div className="surface-toolbar">
        <div><span className="eyebrow">Stage-time validation</span><h2>Semantic checks</h2></div>
        <span className="checks-runtime">{bundle.checks.items.reduce((total, check) => total + check.duration_ms, 0)} ms total</span>
      </div>
      <div className="checks-hero">
        {failed > 0 ? <XCircle aria-hidden="true" /> : <ShieldCheck aria-hidden="true" />}
        <div>
          <strong>{failed > 0 ? `${failed} required check${failed === 1 ? "" : "s"} failed` : "No error-level findings"}</strong>
          <span>{failed > 0 ? "Approval remains blocked until the review bundle is regenerated cleanly." : "Warnings and the independent-review gate remain visible."}</span>
        </div>
      </div>
      <div className="checks-list detailed">
        {bundle.checks.items.map((check) => (
          <article className={`check-row ${check.status}`} key={check.id}>
            <StatusGlyph status={check.status} />
            <div><strong>{check.label}</strong><span>{check.detail}</span></div>
            <code>{check.id}</code>
            <time>{check.duration_ms ? `${check.duration_ms} ms` : "waiting"}</time>
          </article>
        ))}
      </div>
      <PageNotice page={bundle.checks} label="Checks" />
    </div>
  );
}

function ProvenanceView({ bundle }: { bundle: ReviewBundle }) {
  return (
    <div className="view-stack">
      <div className="surface-toolbar">
        <div><span className="eyebrow">Evidence contract</span><h2>Why these edits exist</h2></div>
        <span className="immutable-pill"><LockKeyhole aria-hidden="true" /> append-only anchors</span>
      </div>
      <div className="provenance-grid">
        {bundle.evidence.items.map((evidence) => (
          <article className="evidence-card" key={evidence.id}>
            <div className="evidence-icon"><BookOpen aria-hidden="true" /></div>
            <div className="evidence-body">
              <span>{evidence.source}</span>
              <h3>{evidence.title}</h3>
              <blockquote>“{evidence.excerpt}”</blockquote>
              <div className="evidence-meta"><code>{evidence.locator}</code><time>{formatDate(evidence.captured_at)}</time></div>
            </div>
            <ExternalLink aria-hidden="true" />
          </article>
        ))}
      </div>
      <PageNotice page={bundle.evidence} label="Evidence anchors" />
      <div className="provenance-chain">
        <span className="eyebrow">Attribution chain</span>
        <div>
          <span><Bot /> {bundle.change_set.envelope.producer}</span>
          <ArrowRight />
          <span><FileJson2 /> {bundle.change_set.envelope.batch_id}</span>
          <ArrowRight />
          <span><GitCommitHorizontal /> {shortHash(bundle.repository.head_sha)}</span>
          <ArrowRight />
          <span><Database /> {displaySnapshotHash(bundle)}</span>
        </div>
      </div>
    </div>
  );
}

function RetrievalView({ bundle }: { bundle: ReviewBundle }) {
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
      {mode === "search" && (
        <div className="retrieval-results">
          {bundle.retrieval.search.items.map((result, index) => (
            <article key={result.id}><span className="result-rank">{index + 1}</span><div><strong>{result.title}</strong><span><OntologyKindMark kind={result.kind} /> · score {result.score}</span><p>{result.snippet}</p></div><code>{result.id}</code></article>
          ))}
        </div>
      )}
      {mode === "recall" && (
        <div className="retrieval-results">
          {bundle.retrieval.recall.items.map((result, index) => (
            <article key={result.id}><span className="result-rank memory">{index + 1}</span><div><strong>{result.memory_type} memory</strong><span>decay-aware score {result.score.toFixed(3)}</span><p>{result.content}</p></div><code>{result.id}</code></article>
          ))}
        </div>
      )}
      {mode === "traverse" && (
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

function ActivityView({ bundle }: { bundle: ReviewBundle }) {
  const [draft, setDraft] = useState("");
  const [localNotes, setLocalNotes] = useState<string[]>([]);

  function addLocalNote() {
    const note = draft.trim();
    if (!note) return;
    setLocalNotes((current) => [...current, note]);
    setDraft("");
  }

  return (
    <div className="view-stack">
      <div className="surface-toolbar"><div><span className="eyebrow">Replayable review thread</span><h2>Conversation</h2></div></div>
      <div className="activity-list">
        {bundle.activity.items.map((item) => (
          <article key={item.id} className={item.tone}>
            <div className="avatar">{item.actor.split(":").at(-1)?.slice(0, 1).toUpperCase()}</div>
            <div><div className="activity-heading"><strong>{item.actor}</strong><span>{item.action}</span><time>{formatDate(item.created_at)}</time></div><p>{item.body}</p></div>
          </article>
        ))}
        {localNotes.map((note, index) => (
          <article key={`local-note-${index}`} className="neutral">
            <div className="avatar">Y</div>
            <div>
              <div className="activity-heading"><strong>you</strong><span>added a local note</span><time>this session</time></div>
              <p>{note}</p>
            </div>
          </article>
        ))}
      </div>
      <PageNotice page={bundle.activity} label="Conversation events" />
      <div className="comment-composer">
        <div className="avatar">Y</div>
        <div><textarea aria-label="Review comment" value={draft} onChange={(event) => setDraft(event.target.value)} placeholder="Leave a review note…" /><div><span>Plain text · this session only</span><button className="button primary" type="button" disabled={!draft.trim()} onClick={addLocalNote}>Add local note</button></div></div>
      </div>
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
        <div className="tier-meter"><span style={{ width: `${(bundle.summary.tier_1 / bundle.change_set.operations.length) * 100}%` }} /><i /></div>
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

function ViewSurface({ activeView, bundle, query, onQuery }: { activeView: View; bundle: ReviewBundle; query: string; onQuery: (value: string) => void }) {
  const unavailable =
    (activeView === "changes" && bundle.enrichment_status.semantic_changes === "unavailable") ||
    (activeView === "graph" && bundle.enrichment_status.affected_graph === "unavailable") ||
    (activeView === "provenance" && bundle.enrichment_status.evidence === "unavailable") ||
    (activeView === "retrieval" && bundle.enrichment_status.retrieval === "unavailable") ||
    (activeView === "activity" && bundle.enrichment_status.activity === "unavailable");
  if (unavailable) {
    return (
      <div className="unavailable-surface">
        <Box aria-hidden="true" />
        <strong>{viewLabels[activeView]} unavailable</strong>
        <span>This review bundle did not claim or invent that enrichment.</span>
      </div>
    );
  }
  if (activeView === "graph") return <GraphView bundle={bundle} />;
  if (activeView === "checks") return <ChecksView bundle={bundle} />;
  if (activeView === "provenance") return <ProvenanceView bundle={bundle} />;
  if (activeView === "retrieval") return <RetrievalView bundle={bundle} />;
  if (activeView === "activity") return <ActivityView bundle={bundle} />;
  return <ChangesView bundle={bundle} query={query} onQuery={onQuery} />;
}

function OperationOntologyMark({
  operation,
}: {
  operation: ReviewReport["change_set"]["operations"][number];
}) {
  const record = operation.after ?? operation.before;
  const stringField = (...names: string[]) => {
    for (const name of names) {
      const value = record?.[name];
      if (typeof value === "string") return value;
    }
    return undefined;
  };
  if (operation.target === "entity") {
    return <EntityKindMark className="substrate-chip" kind={stringField("entity_kind", "kind") ?? "entity"} />;
  }
  if (operation.target === "note") {
    return <NoteKindMark className="substrate-chip" kind={stringField("note_kind", "kind") ?? "note"} />;
  }
  return <RelationMark className="substrate-chip edge" relation={stringField("relation") ?? "edge"} />;
}

function CoreReviewStudio({
  report,
  onImport,
  onDownload,
  onUseDemo,
}: {
  report: ReviewReport;
  onImport: () => void;
  onDownload: () => void;
  onUseDemo: () => void;
}) {
  return (
    <div className="app-shell core-report-shell">
      <header className="topbar">
        <div className="brand-lockup">
          <div className="brand-mark" aria-hidden="true"><span /><span /><span /></div>
          <span className="brand-name">khive</span>
          <span className="brand-product">KG Studio</span>
        </div>
        <div className="global-search core-report-label">
          <FileJson2 aria-hidden="true" />
          <span>khive.review.v1 · headless report</span>
          <kbd>read only</kbd>
        </div>
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

        <div className="review-layout core-review-layout">
          <section className="review-surface" aria-label="Ordered change-set operations">
            <div className="view-stack">
              <div className="surface-toolbar"><div><span className="eyebrow">ADR-101 operation order</span><h2>{report.change_set.operations.length} staged operations</h2></div><code>{report.tier_summary.policy}</code></div>
              <div className="change-list core-operation-list">
                {report.change_set.operations.map((operation) => (
                  <article className="change-card selected" key={`${operation.index}-${operation.id}`}>
                    <div className="change-summary core-operation-summary">
                      <span className="change-sign">{operation.index + 1}</span>
                      <div className="change-title"><strong>{operation.summary}</strong><span>{operation.reason}</span></div>
                      <OperationOntologyMark operation={operation} />
                      <TierPill tier={operation.tier} />
                    </div>
                    <div className="change-detail core-operation-detail">
                      <div className="record-id"><Code2 /> {operation.id}</div>
                      <div className="core-operation-meta"><span>{operation.op}</span><span>{operation.entity_ids.length} subject ID{operation.entity_ids.length === 1 ? "" : "s"}</span></div>
                      {operation.tier_reasons.length > 0 && <ul>{operation.tier_reasons.map((reason) => <li key={reason}>{reason}</li>)}</ul>}
                      {operation.before !== undefined && <FieldDiff field={{ path: "preimage", before: operation.before }} />}
                      {operation.after !== undefined && <FieldDiff field={{ path: "projected value", after: operation.after }} />}
                    </div>
                  </article>
                ))}
              </div>
            </div>
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
  const [activeView, setActiveView] = useState<View>("changes");
  const [query, setQuery] = useState("");
  const [reviewerFamily, setReviewerFamily] = useState(bundle.change_set.envelope.producer_model_family);
  const [decision, setDecision] = useState<ReviewDecision>("pending");
  const [toast, setToast] = useState<Toast>(null);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);

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
        />
        {toast && <div className={`toast ${toast.tone}`} role="status"><CheckCircle2 /><span>{toast.message}</span><button type="button" onClick={() => setToast(null)} aria-label="Dismiss"><X /></button></div>}
      </>
    );
  }

  return (
    <div className="app-shell">
      <Header bundle={bundle} onImport={() => fileInput.current?.click()} onDownload={downloadBundle} />
      {filePicker}
      <button className="mobile-menu" type="button" onClick={() => setSidebarOpen((open) => !open)} aria-label="Toggle navigation"><Menu /></button>
      <div className="app-body">
        <div className={`sidebar-wrap ${sidebarOpen ? "open" : ""}`} onClick={() => setSidebarOpen(false)}>
          <Sidebar bundle={bundle} activeView={activeView} onView={(view) => { setActiveView(view); setSidebarOpen(false); }} />
        </div>
        <main className="workspace">
          <CapabilityBanner bundle={bundle} />
          <PullRequestHeader bundle={bundle} onCopy={() => void copyCli()} />
          <WorkspaceTabs activeView={activeView} onView={setActiveView} bundle={bundle} />
          <div className="review-layout">
            <section className="review-surface" aria-label={viewLabels[activeView]}>
              <ViewSurface
                key={`${bundle.repository.owner}/${bundle.repository.name}#${bundle.pull_request.number}@${bundle.pull_request.head_sha}`}
                activeView={activeView}
                bundle={bundle}
                query={query}
                onQuery={setQuery}
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
