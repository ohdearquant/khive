"use client";

import {
  AlertTriangle,
  Boxes,
  Braces,
  CalendarDays,
  ChartNoAxesCombined,
  CircleDot,
  Clock3,
  Code2,
  Copy,
  Database,
  GitBranch,
  GitCommitHorizontal,
  GitFork,
  Info,
  Layers3,
  Network,
  Package,
  Radar,
  ShieldCheck,
  Sparkles,
  TrendingUp,
  Users,
} from "@/icons";
import { useEffect, useMemo, useRef, useState } from "react";

import { DataState } from "@/components/data-state";
import { RepositoryCommandPalette } from "@/components/showcase/repository-command-palette";
import { RepositoryTriage } from "@/components/showcase/repository-triage";
import type { ShowcaseBundleSource } from "@/lib/adapters/preferred-showcase-source";
import {
  DerivedEdgeMark,
  edgeDirectionMark,
  edgeHueStyle,
  EntityKindMark,
  kindHueStyle,
  OntologyLegend,
  RelationMark,
} from "@/components/ontology-mark";
import { settleGraphLayout } from "@/lib/graph-layout";
import { edgeLegendFor, entityLegendFor } from "@/lib/ontology-legend";
import { buildRepositoryBrief } from "@/lib/repository-brief";
import type {
  RepoBundle,
  RepoModule,
  RepoPage,
  ViewId,
} from "@/lib/repo-bundle";
import {
  parseRepositoryLocation,
  REPOSITORY_VIEW_IDS,
  investigationShareUrl,
  repositoryLocationUrl,
} from "@/lib/repository-location";

type Labels = RepoBundle["capability"]["labels"];
type ViewCapability = RepoBundle["capability"]["views"][ViewId];
type Icon = typeof Network;
type ModuleMap = Map<string, RepoModule>;
type ViewProps = Readonly<{
  bundle: RepoBundle;
  moduleById: ModuleMap;
  selectedModuleId: string | null;
  onSelectModule: (moduleId: string) => void;
  onExploreStructure: () => void;
}>;

const viewOrder: readonly ViewId[] = REPOSITORY_VIEW_IDS;

const viewIcons: Record<ViewId, Icon> = {
  structure_graph: Network,
  history_structure_navigation: GitBranch,
  dependency_topology: GitFork,
  hotspot_quadrant: Radar,
  hidden_coupling: Braces,
  structure_treemap: Layers3,
  cadence_timeline: CalendarDays,
  ownership: Users,
  api_surface: Code2,
  scorecard: ChartNoAxesCombined,
};

const UI_ROW_LIMIT = 200;
const UI_TREEMAP_LIMIT = 180;
const UI_RESIDUAL_LIMIT = 80;
const UI_GRAPH_EDGE_LIMIT = 50;

function derivedDiamondPoints(x: number, y: number): string {
  const r = 1.1;
  return `${x},${y - r} ${x + r},${y} ${x},${y + r} ${x - r},${y}`;
}

function formatNumber(value: number): string {
  return new Intl.NumberFormat("en", { notation: value >= 10_000 ? "compact" : "standard" }).format(value);
}

function formatDate(value: string): string {
  return new Intl.DateTimeFormat("en", { month: "short", day: "numeric", year: "numeric" }).format(new Date(value));
}

function formatPercent(value: number): string {
  const normalized = value <= 1 ? value : value / 100;
  return new Intl.NumberFormat("en", { style: "percent", maximumFractionDigits: 1 }).format(normalized);
}

function shortSha(value: string): string {
  return value.slice(0, 8);
}

function moduleName(moduleById: ModuleMap, id: string): string {
  const moduleNode = moduleById.get(id);
  return moduleNode?.module_path ?? moduleNode?.name ?? id;
}

function availabilityText<T>(
  value: { status: "available"; value: T } | { status: "unavailable"; reason: string },
  labels: Labels,
  render: (available: T) => string = String,
): string {
  return value.status === "available" ? render(value.value) : `${labels.unavailable} · ${value.reason}`;
}

function isIncompleteRepoPage<T>(page: RepoPage<T>): boolean {
  return page.disclosure.status !== "unavailable"
    && (page.truncated || page.next_cursor != null || page.disclosure.status === "truncated");
}

function isKnownEmptyRepoPage<T>(page: RepoPage<T>): boolean {
  return page.items.length === 0
    && page.disclosure.status === "complete"
    && !page.truncated
    && page.next_cursor == null;
}

function BoundDisclosure<T>({ page, labels }: { page: RepoPage<T>; labels: Labels }) {
  const total = page.total_count.status === "available" ? formatNumber(page.total_count.value) : labels.unavailable;
  const reason = page.disclosure.reason ?? (page.truncated ? labels.truncated : undefined);
  if (page.disclosure.status === "unavailable") {
    return (
      <DataState
        className="repo-bounded"
        presentation="inline"
        state="unavailable"
        title={labels.unavailable}
        message={reason ?? "This bundle does not claim a complete collection."}
        context={[`${formatNumber(page.items.length)} / ${total}`]}
      />
    );
  }
  if (isIncompleteRepoPage(page)) {
    return (
      <DataState
        className="repo-bounded"
        presentation="inline"
        state="truncated"
        title={labels.truncated}
        shown={page.items.length}
        bound={page.bound.max_items}
        knownTotal={page.total_count.status === "available" ? page.total_count.value : undefined}
        reason={reason ?? "The exported collection reached its declared bound."}
      />
    );
  }
  return (
    <span className="repo-bounded complete">
      <Info aria-hidden="true" />
      <span>{formatNumber(page.items.length)} / {total}</span>
    </span>
  );
}

function InlinePageState<T>({ page, labels }: { page: RepoPage<T>; labels: Labels }) {
  if (page.disclosure.status === "unavailable") {
    return (
      <DataState
        className="repo-inline-state"
        presentation="inline"
        state="unavailable"
        title={labels.unavailable}
        message={page.disclosure.reason ?? "This bundle does not claim a complete collection."}
      />
    );
  }
  if (isIncompleteRepoPage(page)) {
    return (
      <DataState
        className="repo-inline-state"
        presentation="inline"
        state="truncated"
        title={labels.truncated}
        shown={page.items.length}
        bound={page.bound.max_items}
        knownTotal={page.total_count.status === "available" ? page.total_count.value : undefined}
        reason={page.disclosure.reason ?? "The exported collection reached its declared bound."}
      />
    );
  }
  return null;
}

function LocalSliceDisclosure({
  shown,
  total,
  label,
  labels,
}: {
  shown: number;
  total: number;
  label: string;
  labels: Labels;
}) {
  if (shown >= total) return null;
  return (
    <DataState
      className="repo-bounded"
      presentation="inline"
      state="truncated"
      title={`${label} ${labels.truncated.toLocaleLowerCase()}`}
      shown={shown}
      bound={shown}
      knownTotal={total}
      reason="The browser display bound hides the remaining captured rows."
    />
  );
}

function InlineLocalSlice({
  shown,
  total,
  labels,
}: {
  shown: number;
  total: number;
  labels: Labels;
}) {
  if (shown >= total) return null;
  return (
    <DataState
      className="repo-inline-state"
      presentation="inline"
      state="truncated"
      title={labels.truncated}
      shown={shown}
      bound={shown}
      knownTotal={total}
      reason="The browser display bound hides the remaining captured rows."
    />
  );
}

function ViewHeader({ capability }: { capability: ViewCapability }) {
  return (
    <header className="repo-view-header">
      <div>
        <h2>{capability.label}</h2>
        {capability.unavailable_reason && <p>{capability.unavailable_reason}</p>}
      </div>
      <div className="repo-view-tags" aria-label={capability.label}>
        <span className="repo-view-tag"><Layers3 aria-hidden="true" /> <code>{capability.granularity}</code></span>
        <span className={`repo-view-tag ${capability.join === "join" ? "join" : ""}`}><GitFork aria-hidden="true" /> <code>{capability.join}</code></span>
      </div>
    </header>
  );
}

function UnavailableView({ capability, labels }: { capability: ViewCapability; labels: Labels }) {
  return (
    <DataState
      className="repo-empty"
      state="unavailable"
      title={`${capability.label} ${labels.unavailable.toLocaleLowerCase()}`}
      message={capability.unavailable_reason ?? "This bundle does not claim data for the view."}
      context={[`${capability.granularity} · ${capability.join}`]}
    />
  );
}

function ViewFrame({
  capability,
  labels,
  allowPartial = false,
  children,
}: {
  capability: ViewCapability;
  labels: Labels;
  allowPartial?: boolean;
  children: React.ReactNode;
}) {
  return (
    <>
      <ViewHeader capability={capability} />
      {capability.status === "available" || allowPartial ? children : <UnavailableView capability={capability} labels={labels} />}
    </>
  );
}

function StructureGraph({ bundle }: { bundle: RepoBundle }) {
  const { graph, capability } = bundle;
  const labels = capability.labels;
  const [subtreeId, setSubtreeId] = useState(graph.repository.id);
  const [zoom, setZoom] = useState(1);
  const [selectedId, setSelectedId] = useState(graph.repository.id);
  const subtreePackages = subtreeId === graph.repository.id
    ? graph.packages.items
    : graph.packages.items.filter((item) => item.id === subtreeId);
  // Sort by id before truncating so the displayed slice — and therefore the
  // shared seeded layout fed by it — is independent of input array order
  // (ADR-153 D4).
  const displayedPackages = [...subtreePackages].sort((left, right) => left.id.localeCompare(right.id)).slice(0, 8);
  const selectablePackages = graph.packages.items.slice(0, UI_ROW_LIMIT);
  const displayedPackageIds = new Set(displayedPackages.map((item) => item.id));
  const subtreeModules = graph.modules.items.filter((item) => subtreeId === graph.repository.id || item.package_id === subtreeId);
  const displayedModules = subtreeModules.filter((item) => displayedPackageIds.has(item.package_id))
    .sort((left, right) => left.id.localeCompare(right.id))
    .slice(0, 42);
  const visibleIds = new Set([
    graph.repository.id,
    ...displayedPackages.map((item) => item.id),
    ...displayedModules.map((item) => item.id),
  ]);
  const visibleEdges = graph.structure_edges.items.filter((edge) => visibleIds.has(edge.source) && visibleIds.has(edge.target));
  const displayedEdges = visibleEdges.slice(0, UI_GRAPH_EDGE_LIMIT);
  const layoutNodes = [
    { id: graph.repository.id },
    ...displayedPackages.map((item) => ({ id: item.id })),
    ...displayedModules.map((item) => ({ id: item.id })),
  ];
  const layoutEdges = [
    ...displayedPackages.map((item) => ({
      id: `contains-${graph.repository.id}-${item.id}`,
      source: graph.repository.id,
      target: item.id,
    })),
    ...displayedModules.map((item) => ({
      id: `contains-${item.package_id}-${item.id}`,
      source: item.package_id,
      target: item.id,
    })),
    ...displayedEdges.map((edge) => ({ id: edge.id, source: edge.source, target: edge.target })),
  ];
  const positions = new Map(
    settleGraphLayout(layoutNodes, layoutEdges).map((node) => [node.id, { x: node.x, y: node.y }]),
  );
  const degrees = new Map<string, number>();
  const fanIn = new Map<string, number>();
  const fanOut = new Map<string, number>();
  for (const edge of graph.structure_edges.items) {
    degrees.set(edge.source, (degrees.get(edge.source) ?? 0) + 1);
    degrees.set(edge.target, (degrees.get(edge.target) ?? 0) + 1);
    fanOut.set(edge.source, (fanOut.get(edge.source) ?? 0) + 1);
    fanIn.set(edge.target, (fanIn.get(edge.target) ?? 0) + 1);
  }
  const maxDegree = Math.max(1, ...visibleIds.values().map((id) => degrees.get(id) ?? 0));
  const nodeWidth = (id: string) => 92 + ((degrees.get(id) ?? 0) / maxDegree) * 46;
  const selectedModule = displayedModules.find((item) => item.id === selectedId);
  const selectedPackage = displayedPackages.find((item) => item.id === selectedId);
  const selectedEntityKind = selectedModule ? "concept" : "project";
  const visibleLabel = new Map<string, string>([
    [graph.repository.id, graph.repository.label],
    ...displayedPackages.map((item) => [item.id, item.name] as const),
    ...displayedModules.map((item) => [item.id, item.module_path] as const),
  ]);

  return (
    <div className="repo-view-body">
      <div className="repo-card">
        <div className="repo-graph-toolbar">
          <label>
            <span>{labels.node_types.package}</span>
            <select
              aria-label={`${labels.node_types.package} · ${capability.views.structure_graph.label}`}
              value={subtreeId}
              onChange={(event) => {
                setSubtreeId(event.target.value);
                setSelectedId(event.target.value);
              }}
            >
              <option value={graph.repository.id}>{labels.node_types.repository}</option>
              {selectablePackages.map((item) => <option value={item.id} key={item.id}>{item.name}</option>)}
            </select>
          </label>
          <div>
            <button type="button" aria-label={`${capability.views.structure_graph.label} −`} onClick={() => setZoom((value) => Math.max(0.75, value - 0.25))}>−</button>
            <output aria-live="polite">{Math.round(zoom * 100)}%</output>
            <button type="button" aria-label={`${capability.views.structure_graph.label} +`} onClick={() => setZoom((value) => Math.min(1.5, value + 0.25))}>+</button>
          </div>
        </div>
        <OntologyLegend
          className="repo-ontology-legend"
          presentEntityKinds={["project", "concept"]}
          presentRelations={displayedEdges.map((edge) => edge.relation)}
        />
        <div className="repo-graph-stage" aria-label={capability.views.structure_graph.label}>
          <div className="repo-graph-viewport" style={{ transform: `scale(${zoom})` }}>
            <svg className="repo-edges" viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
              <defs>
                <marker id="showcase-ontology-arrow" markerHeight="6" markerWidth="6" orient="auto" refX="5" refY="3" viewBox="0 0 6 6">
                  <path d="M 0 0 L 6 3 L 0 6 z" fill="context-stroke" />
                </marker>
              </defs>
              {displayedEdges.map((edge) => {
                const source = positions.get(edge.source);
                const target = positions.get(edge.target);
                if (!source || !target) return null;
                const legend = edgeLegendFor(edge.relation);
                const direction = edgeDirectionMark(legend, source, target);
                return (
                  <g key={edge.id}>
                    <line
                      className="ontology-edge"
                      data-edge-family={legend.family}
                      data-edge-origin={edge.origin}
                      data-edge-treatment={legend.treatment}
                      data-edge-variant={legend.variant}
                      markerEnd={legend.directed ? "url(#showcase-ontology-arrow)" : undefined}
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
                    {edge.origin === "derived" && (
                      <polygon
                        className="ontology-derived-glyph"
                        points={derivedDiamondPoints(
                          source.x + (target.x - source.x) * 0.4,
                          source.y + (target.y - source.y) * 0.4,
                        )}
                      />
                    )}
                  </g>
                );
              })}
            </svg>
            <button
              className={`repo-graph-node ${selectedId === graph.repository.id ? "selected" : ""}`}
              data-node-id={graph.repository.id}
              style={{
                left: `${positions.get(graph.repository.id)!.x}%`,
                top: `${positions.get(graph.repository.id)!.y}%`,
                width: `${nodeWidth(graph.repository.id)}px`,
                ...kindHueStyle(entityLegendFor("project")),
              }}
              type="button"
              aria-pressed={selectedId === graph.repository.id}
              onClick={() => setSelectedId(graph.repository.id)}
            >
              <EntityKindMark className="repo-node-kind-icon" kind="project" showLabel={false} />
              <span>{labels.node_types.repository}</span><strong>{graph.repository.label}</strong>
            </button>
            {displayedPackages.map((item) => {
              const position = positions.get(item.id)!;
              return (
                <button className={`repo-graph-node ${selectedId === item.id ? "selected" : ""}`} data-node-id={item.id} style={{ left: `${position.x}%`, top: `${position.y}%`, width: `${nodeWidth(item.id)}px`, ...kindHueStyle(entityLegendFor("project")) }} type="button" aria-pressed={selectedId === item.id} key={item.id} onClick={() => setSelectedId(item.id)}>
                  <EntityKindMark className="repo-node-kind-icon" kind="project" showLabel={false} />
                  <span>{labels.node_types.package}</span><strong>{item.name}</strong>
                </button>
              );
            })}
            {displayedModules.map((item) => {
              const position = positions.get(item.id)!;
              return (
                <button className={`repo-graph-node ${selectedId === item.id ? "selected" : ""}`} data-node-id={item.id} style={{ left: `${position.x}%`, top: `${position.y}%`, width: `${nodeWidth(item.id)}px`, ...kindHueStyle(entityLegendFor("concept")) }} type="button" aria-pressed={selectedId === item.id} key={item.id} onClick={() => setSelectedId(item.id)}>
                  <EntityKindMark className="repo-node-kind-icon" kind="concept" showLabel={false} />
                  <span>{labels.node_types.module}</span><strong>{item.module_path}</strong>
                </button>
              );
            })}
          </div>
        </div>
        <div className="repo-inspector">
          <div className="repo-inspector-heading">
            <EntityKindMark kind={selectedEntityKind} showLabel={false} />
            <div>
              <span>{selectedModule ? labels.node_types.module : selectedPackage ? labels.node_types.package : labels.node_types.repository}</span>
              <strong>{selectedModule?.module_path ?? selectedPackage?.name ?? graph.repository.label}</strong>
            </div>
            {selectedModule && <code>{selectedModule.source_path}</code>}
          </div>
          <div className="repo-inspector-metrics">
            <span>{labels.metrics.fan_in}<strong>{formatNumber(fanIn.get(selectedId) ?? 0)}</strong></span>
            <span>{labels.metrics.fan_out}<strong>{formatNumber(fanOut.get(selectedId) ?? 0)}</strong></span>
          </div>
        </div>
        <ul className="repo-edge-list" aria-label={capability.views.structure_graph.label}>
          {displayedEdges.map((edge) => (
            <li key={`${edge.id}-accessible`}>
              <code>{visibleLabel.get(edge.source) ?? edge.source}</code><RelationMark relation={edge.relation} /><code>{visibleLabel.get(edge.target) ?? edge.target}</code><em>{edge.origin === "derived" ? <DerivedEdgeMark label={labels.derived} /> : labels.ingested}</em>
            </li>
          ))}
        </ul>
        <LocalSliceDisclosure shown={displayedEdges.length} total={graph.structure_edges.items.length} label={capability.views.structure_graph.label} labels={labels} />
        <LocalSliceDisclosure shown={displayedPackages.length} total={subtreePackages.length} label={labels.node_types.package} labels={labels} />
        <LocalSliceDisclosure shown={displayedModules.length} total={subtreeModules.length} label={labels.node_types.module} labels={labels} />
      </div>
      <BoundDisclosure page={graph.packages} labels={labels} />
      <BoundDisclosure page={graph.modules} labels={labels} />
      <BoundDisclosure page={graph.structure_edges} labels={labels} />
    </div>
  );
}

function FacetState({
  label,
  value,
  labels,
}: {
  label: string;
  value: { status: "available"; value: boolean } | { status: "unavailable"; reason: string };
  labels: Labels;
}) {
  return (
    <div className="repo-stat-line">
      <span>{label}</span>
      <strong aria-label={value.status === "available" ? `${label}: ${String(value.value)}` : labels.unavailable}>
        {value.status === "available" ? <code>{String(value.value)}</code> : labels.unavailable}
      </strong>
    </div>
  );
}

type HistoryFacetValue = RepoBundle["graph"]["history_navigation"]["by_module"]["items"][number]["pull_requests"];

function HistoryFacet({
  label,
  icon: Icon,
  value,
  parentPage,
  resolveItem,
  labels,
  onExploreStructure,
}: {
  label: string;
  icon: Icon;
  value?: HistoryFacetValue;
  parentPage: RepoBundle["graph"]["history_navigation"]["by_module"];
  resolveItem: (id: string) => { title: string; number: number } | undefined;
  labels: Labels;
  onExploreStructure: () => void;
}) {
  const page = value?.status === "available" ? value.value : null;
  const rows = page?.items.slice(0, UI_ROW_LIMIT) ?? [];
  const unavailableReason = value?.status === "unavailable"
    ? value.reason
    : !value && parentPage.disclosure.status === "unavailable"
      ? parentPage.disclosure.reason
      : null;

  return (
    <section className="repo-card">
      <div className="repo-card-heading"><h3>{label}</h3></div>
      {unavailableReason ? (
        <DataState className="repo-empty" state="unavailable" title={`${label} ${labels.unavailable.toLocaleLowerCase()}`} message={unavailableReason} />
      ) : page && isKnownEmptyRepoPage(page) ? (
        <DataState className="repo-empty" state="empty" title={`No ${label}`} message={`${label} captured for the selected module belong here.`} action={{ label: "Explore repository structure", onClick: onExploreStructure }} />
      ) : rows.length > 0 ? (
        <div className="repo-list">
          {rows.map((id) => {
            const item = resolveItem(id);
            return <div className="repo-list-row" key={id}><Icon aria-hidden="true" /><div><strong>{item?.title ?? id}</strong>{item && <span>#{item.number}</span>}</div></div>;
          })}
        </div>
      ) : null}
      {page && <LocalSliceDisclosure shown={rows.length} total={page.items.length} label={label} labels={labels} />}
      {page && <BoundDisclosure page={page} labels={labels} />}
    </section>
  );
}

function HistoryStructure({
  bundle,
  selectedModuleId,
  onSelectModule,
  onExploreStructure,
}: ViewProps) {
  const { graph, capability } = bundle;
  const labels = capability.labels;
  const view = capability.views.history_structure_navigation;
  const [commitSelection, setCommitSelection] = useState<{
    moduleId: string | null;
    commitId: string;
  }>({ moduleId: null, commitId: "" });
  const selectedCommitId = commitSelection.moduleId === selectedModuleId
    ? commitSelection.commitId
    : "";
  const selectedModuleNavigation = graph.history_navigation.by_module.items.find((item) => item.module_id === selectedModuleId);
  const selectedCommitNavigation = graph.history_navigation.by_commit.items.find((item) => item.commit_id === selectedCommitId);
  const linkedCommitIds = new Set(selectedModuleNavigation?.commits.items ?? []);
  const commits = graph.commits.items.filter((commit) => linkedCommitIds.has(commit.id)).slice(0, UI_ROW_LIMIT);
  const linkedModuleIds = new Set(selectedCommitNavigation?.modules.items ?? []);
  const modules = (selectedCommitId
    ? graph.modules.items.filter((module) => linkedModuleIds.has(module.id))
    : graph.modules.items).slice(0, 100);
  const resolutions = graph.join_resolution.repositories.status === "available"
    ? graph.join_resolution.repositories.value.slice(0, UI_ROW_LIMIT)
    : [];
  const historicalResolutions = graph.join_resolution.historical.status === "available"
    ? graph.join_resolution.historical.value.slice(0, UI_ROW_LIMIT)
    : [];

  return (
    <div className="repo-view-body repo-grid">
      <div className="repo-grid history">
        <section className="repo-card" data-history-modules>
          <div className="repo-card-heading"><h3>{labels.node_types.module}</h3><p>{formatNumber(modules.length)}</p></div>
          <div className="repo-list">
            {modules.map((module) => (
              <button type="button" data-module-id={module.id} aria-pressed={selectedModuleId === module.id} className={`repo-list-row ${selectedModuleId === module.id ? "selected" : ""}`} key={module.id} onClick={() => { onSelectModule(module.id); setCommitSelection({ moduleId: module.id, commitId: "" }); }}>
                <Boxes aria-hidden="true" /><div><strong>{module.module_path}</strong><span>{module.source_path}</span></div>
              </button>
            ))}
          </div>
          <LocalSliceDisclosure shown={Math.min(100, modules.length)} total={selectedCommitId ? linkedModuleIds.size : modules.length} label={labels.node_types.module} labels={labels} />
        </section>
        <section className="repo-card" data-history-commits>
          <div className="repo-card-heading"><h3>{labels.node_types.commit}</h3><p>{formatNumber(commits.length)}</p></div>
          <div className="repo-list">
            {commits.map((commit) => (
              <button type="button" data-commit-id={commit.id} aria-pressed={selectedCommitId === commit.id} className={`repo-list-row ${selectedCommitId === commit.id ? "selected" : ""}`} key={commit.id} onClick={() => setCommitSelection({ moduleId: selectedModuleId, commitId: commit.id })}>
                <GitCommitHorizontal aria-hidden="true" /><div><strong>{commit.subject}</strong><span>{commit.author} · {formatDate(commit.committed_at)}</span></div><code>{commit.short_sha}</code>
              </button>
            ))}
            {commits.length === 0 && (
              selectedModuleNavigation?.commits.disclosure.status === "unavailable" ||
              graph.history_navigation.by_module.disclosure.status === "unavailable" ||
              graph.commits.disclosure.status === "unavailable"
                ? <DataState className="repo-empty" state="unavailable" title={`${labels.node_types.commit} ${labels.unavailable.toLocaleLowerCase()}`} message={selectedModuleNavigation?.commits.disclosure.reason ?? graph.history_navigation.by_module.disclosure.reason ?? "This bundle does not claim commit navigation."} />
                : linkedCommitIds.size > 0
                  ? <DataState className="repo-empty" state="truncated" title={`${labels.node_types.commit} ${labels.truncated.toLocaleLowerCase()}`} shown={commits.length} bound={graph.commits.bound.max_items} knownTotal={linkedCommitIds.size} reason={graph.commits.disclosure.reason ?? "Referenced commits fall outside the captured graph bound."} />
                  : selectedModuleNavigation && isKnownEmptyRepoPage(selectedModuleNavigation.commits)
                    ? <DataState className="repo-empty" state="empty" title={`No ${labels.node_types.commit}`} message="Commits captured for the selected module belong here." action={{ label: "Explore repository structure", onClick: onExploreStructure }} />
                    : null
            )}
          </div>
          <LocalSliceDisclosure shown={commits.length} total={linkedCommitIds.size} label={labels.node_types.commit} labels={labels} />
          {selectedModuleNavigation && <BoundDisclosure page={selectedModuleNavigation.commits} labels={labels} />}
          {selectedCommitNavigation && <BoundDisclosure page={selectedCommitNavigation.modules} labels={labels} />}
        </section>
      </div>
      <div className="repo-grid two">
        <HistoryFacet
          label={labels.node_types.pull_request}
          icon={GitBranch}
          value={selectedModuleNavigation?.pull_requests}
          parentPage={graph.history_navigation.by_module}
          resolveItem={(id) => graph.pull_requests.items.find((candidate) => candidate.id === id)}
          labels={labels}
          onExploreStructure={onExploreStructure}
        />
        <HistoryFacet
          label={labels.node_types.issue}
          icon={CircleDot}
          value={selectedModuleNavigation?.issues}
          parentPage={graph.history_navigation.by_module}
          resolveItem={(id) => graph.issues.items.find((candidate) => candidate.id === id)}
          labels={labels}
          onExploreStructure={onExploreStructure}
        />
      </div>
      <div className="repo-grid two">
        <section className="repo-card" data-join-resolution>
          <div className="repo-card-heading"><h3>{labels.derived}</h3><p>{graph.commit_module_edges.bound.order}</p></div>
          <div style={{ padding: 14 }}>
            <span className="repo-derived-badge"><Sparkles aria-hidden="true" /> {labels.derived}</span>
            {resolutions.map((resolution) => (
              <div key={`${resolution.repository}-${resolution.language}`}>
                <div className="repo-stat-line"><span>{labels.metrics.resolution}</span><strong>{availabilityText(resolution.resolution_rate, labels, formatPercent)}</strong></div>
                <div className="repo-stat-line"><span>{labels.metrics.source_files}</span><strong>{formatNumber(resolution.files)}</strong></div>
                <div className="repo-stat-line"><span>{labels.node_types.module}</span><strong>{formatNumber(resolution.entity_keys)}</strong></div>
                <ul className="repo-residuals">{resolution.residuals.items.slice(0, UI_RESIDUAL_LIMIT).map((residual) => <li key={`${residual.side}-${residual.source_path}-${residual.module_path}`}>{residual.source_path || residual.module_path} · {residual.reason}</li>)}</ul>
                <LocalSliceDisclosure shown={Math.min(UI_RESIDUAL_LIMIT, resolution.residuals.items.length)} total={resolution.residuals.items.length} label={labels.metrics.resolution} labels={labels} />
                <BoundDisclosure page={resolution.residuals} labels={labels} />
              </div>
            ))}
            {historicalResolutions.map((historical) => (
              <div key={`${historical.repository}-${historical.language}`}>
                <div className="repo-stat-line"><span>{labels.metrics.change_frequency}</span><strong>{formatNumber(historical.total_changed_paths)}</strong></div>
                <div className="repo-stat-line"><span>{labels.metrics.resolution}</span><strong>{formatNumber(historical.matched_rust_paths)} / {formatNumber(historical.rust_in_scope_paths)}</strong></div>
                <ul className="repo-residuals">{historical.unresolved_rust_paths.items.slice(0, UI_RESIDUAL_LIMIT).map((residual) => <li key={`${residual.commit_sha}-${residual.source_path}`}>{shortSha(residual.commit_sha)} · {residual.source_path} · {residual.reason}</li>)}</ul>
                <LocalSliceDisclosure shown={Math.min(UI_RESIDUAL_LIMIT, historical.unresolved_rust_paths.items.length)} total={historical.unresolved_rust_paths.items.length} label={labels.metrics.resolution} labels={labels} />
                <BoundDisclosure page={historical.unresolved_rust_paths} labels={labels} />
              </div>
            ))}
            {graph.join_resolution.repositories.status === "available" && <LocalSliceDisclosure shown={resolutions.length} total={graph.join_resolution.repositories.value.length} label={labels.metrics.resolution} labels={labels} />}
            {graph.join_resolution.historical.status === "available" && <LocalSliceDisclosure shown={historicalResolutions.length} total={graph.join_resolution.historical.value.length} label={labels.metrics.change_frequency} labels={labels} />}
            {graph.join_resolution.repositories.status === "unavailable" && <DataState className="repo-empty" state="unavailable" title={`${labels.metrics.resolution} ${labels.unavailable.toLocaleLowerCase()}`} message={graph.join_resolution.repositories.reason} />}
            {graph.join_resolution.historical.status === "unavailable" && <div className="repo-stat-line"><span>{labels.metrics.change_frequency}</span><strong>{labels.unavailable}</strong></div>}
          </div>
        </section>
        <section className="repo-card" data-history-capabilities>
          <div className="repo-card-heading"><h3>{view.label}</h3><p>{view.join}</p></div>
          <div style={{ padding: "4px 14px 12px" }}>
            <FacetState label={labels.node_types.commit} value={view.commit_module_facet} labels={labels} />
            <FacetState label={labels.node_types.pull_request} value={view.pull_request_module_facet} labels={labels} />
            <FacetState label={labels.node_types.issue} value={view.issue_module_facet} labels={labels} />
          </div>
        </section>
      </div>
      <BoundDisclosure page={graph.commit_module_edges} labels={labels} />
      <BoundDisclosure page={graph.history_navigation.by_module} labels={labels} />
      <BoundDisclosure page={graph.history_navigation.by_commit} labels={labels} />
      <BoundDisclosure page={graph.modules} labels={labels} />
      <BoundDisclosure page={graph.commits} labels={labels} />
      <BoundDisclosure page={graph.pull_requests} labels={labels} />
      <BoundDisclosure page={graph.issues} labels={labels} />
    </div>
  );
}

function DependencyTopology({ bundle, moduleById, onExploreStructure }: ViewProps) {
  const analysis = bundle.aggregates.dependency_topology;
  const labels = bundle.capability.labels;
  const moduleRows = analysis.modules.items.slice(0, UI_ROW_LIMIT);
  const cycleRows = analysis.cycles.items.slice(0, UI_ROW_LIMIT);
  return (
    <div className="repo-view-body repo-grid two">
      <section className="repo-card repo-table-wrap">
        <table className="repo-table">
          <thead><tr><th>{labels.node_types.module}</th><th>{labels.metrics.fan_in}</th><th>{labels.metrics.fan_out}</th><th>{labels.metrics.cycle_count}</th></tr></thead>
          <tbody>{moduleRows.map((row) => <tr key={row.module_id}><td><strong>{moduleName(moduleById, row.module_id)}</strong></td><td>{formatNumber(row.fan_in)}</td><td>{formatNumber(row.fan_out)}</td><td>{formatNumber(row.cycle_ids.length)}</td></tr>)}</tbody>
        </table>
        <LocalSliceDisclosure shown={moduleRows.length} total={analysis.modules.items.length} label={labels.node_types.module} labels={labels} />
        <BoundDisclosure page={analysis.modules} labels={labels} />
      </section>
      <section className="repo-card">
        <div className="repo-card-heading"><h3>{labels.metrics.cycle_count}</h3><p>{formatNumber(analysis.cycles.items.length)}</p></div>
        <div className="repo-list">{cycleRows.map((cycle) => <div className="repo-list-row" key={cycle.id}><GitFork aria-hidden="true" /><div><strong>{cycle.id}</strong><span>SCC members: {cycle.module_ids.map((id) => moduleName(moduleById, id)).join(" · ")}</span></div></div>)}</div>
        {isKnownEmptyRepoPage(analysis.cycles) && <DataState className="repo-empty" state="empty" title="No dependency cycles in this bundle" message="Dependency cycles found by the captured topology analysis belong here." action={{ label: "Explore repository structure", onClick: onExploreStructure }} />}
        <LocalSliceDisclosure shown={cycleRows.length} total={analysis.cycles.items.length} label={labels.metrics.cycle_count} labels={labels} />
        <BoundDisclosure page={analysis.cycles} labels={labels} />
      </section>
    </div>
  );
}

function HotspotQuadrantView({ bundle, moduleById }: { bundle: RepoBundle; moduleById: ModuleMap }) {
  const analysis = bundle.aggregates.hotspot_quadrant;
  const labels = bundle.capability.labels;
  const rows = analysis.data.items.slice(0, UI_ROW_LIMIT);
  const maxFanIn = Math.max(1, ...rows.map((row) => row.fan_in));
  const maxChanges = Math.max(1, ...rows.map((row) => row.commit_count));
  return (
    <div className="repo-view-body repo-grid">
      <div className="repo-chart">
        <svg data-visualization="hotspot" viewBox="0 0 100 70" role="img" aria-labelledby="hotspot-title hotspot-desc">
          <title id="hotspot-title">{bundle.capability.views.hotspot_quadrant.label}</title>
          <desc id="hotspot-desc">{analysis.meta.inputs.join(", ")}</desc>
          <line className="repo-chart-grid" x1="50" y1="4" x2="50" y2="64" /><line className="repo-chart-grid" x1="8" y1="34" x2="96" y2="34" />
          <text className="repo-chart-axis" x="50" y="69" textAnchor="middle">{labels.metrics.fan_in}</text>
          <text className="repo-chart-axis" transform="rotate(-90 2 35)" x="2" y="35" textAnchor="middle">{labels.metrics.change_frequency}</text>
          {rows.map((row) => {
            const x = 8 + (row.fan_in / maxFanIn) * 86;
            const y = 64 - (row.commit_count / maxChanges) * 58;
            return <circle key={row.module_id} className={`repo-chart-dot ${row.quadrant === "high_churn_high_fan_in" ? "hot" : ""}`} cx={x} cy={y} r="2.3"><title>{moduleName(moduleById, row.module_id)} · {labels.metrics.change_frequency}: {row.commit_count} · {labels.metrics.fan_in}: {row.fan_in} · {labels.hotspot_quadrants[row.quadrant]}</title></circle>;
          })}
        </svg>
      </div>
      <section className="repo-card repo-table-wrap">
        <table className="repo-table"><thead><tr><th>{labels.node_types.module}</th><th>{labels.metrics.change_frequency}</th><th>{labels.metrics.fan_in}</th><th>{bundle.capability.views.hotspot_quadrant.label}</th></tr></thead><tbody>{rows.map((row) => <tr key={row.module_id}><td><strong>{moduleName(moduleById, row.module_id)}</strong></td><td>{row.commit_count}</td><td>{row.fan_in}</td><td>{labels.hotspot_quadrants[row.quadrant]}</td></tr>)}</tbody></table>
        <LocalSliceDisclosure shown={rows.length} total={analysis.data.items.length} label={bundle.capability.views.hotspot_quadrant.label} labels={labels} />
        <BoundDisclosure page={analysis.data} labels={labels} />
      </section>
    </div>
  );
}

function HiddenCouplingView({ bundle, moduleById, onExploreStructure }: ViewProps) {
  const analysis = bundle.aggregates.hidden_coupling;
  const labels = bundle.capability.labels;
  const rows = analysis.data.items.slice(0, UI_ROW_LIMIT);
  return (
    <div className="repo-view-body">
      <section className="repo-card repo-table-wrap">
        <table className="repo-table"><thead><tr><th>{labels.node_types.module}</th><th>{labels.node_types.module}</th><th>{labels.metrics.cochange_count}</th><th>{labels.metrics.support}</th></tr></thead><tbody>{rows.map((row) => <tr key={`${row.left_module_id}-${row.right_module_id}`}><td><strong>{moduleName(moduleById, row.left_module_id)}</strong></td><td><strong>{moduleName(moduleById, row.right_module_id)}</strong></td><td>{formatNumber(row.cochange_count)}</td><td><div className="repo-bar violet" aria-label={`${labels.metrics.support} ${formatPercent(row.support)}`}><span style={{ width: `${Math.min(100, row.support * 100)}%` }} /></div></td></tr>)}</tbody></table>
        {isKnownEmptyRepoPage(analysis.data) && <DataState className="repo-empty" state="empty" title={`No ${bundle.capability.views.hidden_coupling.label.toLocaleLowerCase()} in this bundle`} message="Module pairs with captured co-change signals belong here." action={{ label: "Explore repository structure", onClick: onExploreStructure }} />}
        <LocalSliceDisclosure shown={rows.length} total={analysis.data.items.length} label={bundle.capability.views.hidden_coupling.label} labels={labels} />
        <BoundDisclosure page={analysis.data} labels={labels} />
      </section>
    </div>
  );
}

function TreemapView({ bundle, moduleById }: { bundle: RepoBundle; moduleById: ModuleMap }) {
  const analysis = bundle.aggregates.structure_treemap;
  const labels = bundle.capability.labels;
  const rows = analysis.data.items.slice(0, UI_TREEMAP_LIMIT);
  const maxActivity = Math.max(1, ...rows.map((row) => row.recent_commit_count.status === "available" ? row.recent_commit_count.value : 0));
  return (
    <div className="repo-view-body">
      <div className="repo-legend"><span><i className="green" />{labels.metrics.source_files}</span><span><i className="red" />{labels.metrics.recent_activity}</span></div>
      <div className="repo-treemap" role="list" aria-label={bundle.capability.views.structure_treemap.label}>
        {rows.map((row) => {
          const activity = row.recent_commit_count.status === "available" ? row.recent_commit_count.value : 0;
          const span = Math.min(6, Math.max(2, row.source_file_count));
          return <div role="listitem" style={{ gridColumn: `span ${span}` }} key={row.module_id}><article className={activity > maxActivity * 0.55 ? "hot" : ""}><strong>{moduleName(moduleById, row.module_id)}</strong><span>{labels.metrics.source_files}: {row.source_file_count}</span><span>{labels.metrics.recent_activity}: {availabilityText(row.recent_commit_count, labels, formatNumber)}</span></article></div>;
        })}
      </div>
      <LocalSliceDisclosure shown={rows.length} total={analysis.data.items.length} label={bundle.capability.views.structure_treemap.label} labels={labels} />
      <BoundDisclosure page={analysis.data} labels={labels} />
    </div>
  );
}

type CadencePage = RepoBundle["aggregates"]["cadence_timeline"]["commits"];
type CadenceSeriesId = "commits" | "issues_opened" | "issues_closed" | "pull_requests_opened" | "pull_requests_merged";

function CadenceSeries({ id, page, label, labels, onExploreStructure }: { id: CadenceSeriesId; page: CadencePage; label: string; labels: Labels; onExploreStructure: () => void }) {
  const rows = page.items.slice(0, UI_ROW_LIMIT);
  const seriesStatus = page.disclosure.status === "complete" && isIncompleteRepoPage(page)
    ? "truncated"
    : page.disclosure.status;
  return (
    <section className="repo-card repo-table-wrap" data-cadence-series={id} data-series-status={seriesStatus}>
      <div className="repo-card-heading"><h3>{label}</h3><p>{page.total_count.status === "available" ? formatNumber(page.total_count.value) : labels.unavailable}</p></div>
      {page.disclosure.status === "unavailable" ? (
        <DataState className="repo-empty compact" state="unavailable" title={`${label} ${labels.unavailable.toLocaleLowerCase()}`} message={page.disclosure.reason ?? "This bundle does not claim cadence data."} />
      ) : isKnownEmptyRepoPage(page) ? (
        <DataState className="repo-empty compact" state="empty" title={`No ${label.toLocaleLowerCase()} cadence points`} message={`Captured weekly ${label.toLocaleLowerCase()} counts belong here.`} action={{ label: "Explore repository structure", onClick: onExploreStructure }} />
      ) : (
        <table className="repo-table"><thead><tr><th>{labels.metrics.week}</th><th>{label}</th></tr></thead><tbody>{rows.map((point) => <tr key={point.week_start}><td>{point.week_start}</td><td>{formatNumber(point.count)}</td></tr>)}</tbody></table>
      )}
      <LocalSliceDisclosure shown={rows.length} total={page.items.length} label={label} labels={labels} />
      <BoundDisclosure page={page} labels={labels} />
    </section>
  );
}

function CadenceView({ bundle, onExploreStructure }: ViewProps) {
  const analysis = bundle.aggregates.cadence_timeline;
  const labels = bundle.capability.labels;
  const commitRows = analysis.commits.items.slice(0, UI_ROW_LIMIT);
  const maxCommits = Math.max(1, ...commitRows.map((point) => point.count));
  const width = Math.max(100, commitRows.length * 8);
  const releaseTags = analysis.release_tags.items.slice(0, UI_ROW_LIMIT);
  return (
    <div className="repo-view-body repo-grid">
      <div className="repo-chart">
        <div className="repo-legend"><span><i className="green" />{labels.metrics.commits}</span></div>
        <svg data-visualization="cadence" viewBox={`0 0 ${width} 70`} role="img" aria-labelledby="cadence-title cadence-desc">
          <title id="cadence-title">{bundle.capability.views.cadence_timeline.label}</title><desc id="cadence-desc">{analysis.meta.inputs.join(", ")}</desc>
          {commitRows.map((point, index) => {
            const x = index * 8 + 4;
            const height = (point.count / maxCommits) * 52;
            return <rect className="repo-chart-bar" key={point.week_start} x={x} y={60 - height} width="5" height={height}><title>{point.week_start} · {labels.metrics.commits}: {point.count}</title></rect>;
          })}
          <text className="repo-chart-axis" x={width / 2} y="68" textAnchor="middle">{labels.metrics.week}</text>
        </svg>
        <LocalSliceDisclosure shown={commitRows.length} total={analysis.commits.items.length} label={labels.metrics.commits} labels={labels} />
      </div>
      <div className="repo-cadence-series">
        <CadenceSeries id="commits" page={analysis.commits} label={labels.metrics.commits} labels={labels} onExploreStructure={onExploreStructure} />
        <CadenceSeries id="issues_opened" page={analysis.issues_opened} label={labels.metrics.issues_opened} labels={labels} onExploreStructure={onExploreStructure} />
        <CadenceSeries id="issues_closed" page={analysis.issues_closed} label={labels.metrics.issues_closed} labels={labels} onExploreStructure={onExploreStructure} />
        <CadenceSeries id="pull_requests_opened" page={analysis.pull_requests_opened} label={labels.metrics.pull_requests_opened} labels={labels} onExploreStructure={onExploreStructure} />
        <CadenceSeries id="pull_requests_merged" page={analysis.pull_requests_merged} label={labels.metrics.pull_requests_merged} labels={labels} onExploreStructure={onExploreStructure} />
      </div>
      <div className="repo-grid two">
        <section className="repo-card" style={{ padding: 14 }}><span className="repo-eyebrow">{labels.metrics.lead_time}</span><strong>{availabilityText(analysis.pull_request_lead_time_hours, labels, (value) => `${labels.metrics.p50} ${value.p50.toFixed(1)} · ${labels.metrics.p90} ${value.p90.toFixed(1)} · ${labels.metrics.p95} ${value.p95.toFixed(1)}`)}</strong></section>
        <section className="repo-card"><div className="repo-list">{releaseTags.map((tag) => <div className="repo-list-row" key={`${tag.name}-${tag.target_sha}`}><GitBranch aria-hidden="true" /><div><strong>{tag.name}</strong><span>{availabilityText(tag.committed_at, labels, formatDate)}</span></div><code>{shortSha(tag.target_sha)}</code></div>)}</div><LocalSliceDisclosure shown={releaseTags.length} total={analysis.release_tags.items.length} label={bundle.capability.views.cadence_timeline.label} labels={labels} /><BoundDisclosure page={analysis.release_tags} labels={labels} /></section>
      </div>
    </div>
  );
}

function OwnershipView({ bundle, moduleById }: { bundle: RepoBundle; moduleById: ModuleMap }) {
  const analysis = bundle.aggregates.ownership;
  const labels = bundle.capability.labels;
  const moduleRows = analysis.modules.items.slice(0, UI_ROW_LIMIT);
  const repositoryAuthors = analysis.repository_authors.items.slice(0, UI_ROW_LIMIT);
  return (
    <div className="repo-view-body repo-grid">
      <div className="repo-grid three">
        <section className="repo-score-card">
          <span>{labels.metrics.author_concentration}</span>
          <div className="repo-score-value"><Users aria-hidden="true" /><strong>{availabilityText(analysis.repository_author_concentration, labels, formatPercent)}</strong></div>
          {analysis.repository_author_concentration.status === "unavailable" && <p>{analysis.repository_author_concentration.reason}</p>}
        </section>
        <section className="repo-score-card">
          <span>{labels.metrics.bus_factor}</span>
          <div className="repo-score-value"><ShieldCheck aria-hidden="true" /><strong>{availabilityText(analysis.repository_bus_factor, labels, formatNumber)}</strong></div>
          {analysis.repository_bus_factor.status === "unavailable" && <p>{analysis.repository_bus_factor.reason}</p>}
        </section>
        <section className="repo-card">
          <div className="repo-list">{repositoryAuthors.map((author) => <div className="repo-list-row" key={author.author}><Users aria-hidden="true" /><div><strong>{author.author}</strong><span>{labels.metrics.commits}: {formatNumber(author.commits)} · {formatPercent(author.share)}</span></div></div>)}</div>
          <LocalSliceDisclosure shown={repositoryAuthors.length} total={analysis.repository_authors.items.length} label={labels.metrics.author_concentration} labels={labels} />
          <BoundDisclosure page={analysis.repository_authors} labels={labels} />
        </section>
      </div>
      <section className="repo-card repo-table-wrap">
        {analysis.modules.disclosure.status === "unavailable" ? <DataState className="repo-empty" state="unavailable" title={`${labels.node_types.module} ${labels.unavailable.toLocaleLowerCase()}`} message={analysis.modules.disclosure.reason ?? "This bundle does not claim ownership modules."} /> : <table className="repo-table"><thead><tr><th>{labels.node_types.module}</th><th>{labels.metrics.commits}</th><th>{labels.metrics.author_concentration}</th><th>{labels.metrics.bus_factor}</th></tr></thead><tbody>{moduleRows.map((row) => <tr key={row.module_id}><td><strong>{moduleName(moduleById, row.module_id)}</strong><div>{row.authors.items.slice(0, 8).map((author) => `${author.author} ${formatPercent(author.share)}`).join(" · ")}</div><InlineLocalSlice shown={Math.min(8, row.authors.items.length)} total={row.authors.items.length} labels={labels} /><InlinePageState page={row.authors} labels={labels} /></td><td>{row.commit_count}</td><td>{row.author_concentration.status === "available" ? <div className="repo-bar" aria-label={`${labels.metrics.author_concentration} ${formatPercent(row.author_concentration.value)}`}><span style={{ width: `${Math.min(100, row.author_concentration.value * 100)}%` }} /></div> : availabilityText(row.author_concentration, labels)}</td><td>{availabilityText(row.bus_factor, labels, formatNumber)}</td></tr>)}</tbody></table>}
        <LocalSliceDisclosure shown={moduleRows.length} total={analysis.modules.items.length} label={labels.node_types.module} labels={labels} />
        <BoundDisclosure page={analysis.modules} labels={labels} />
      </section>
    </div>
  );
}

function ApiSurfaceView({ bundle, moduleById }: { bundle: RepoBundle; moduleById: ModuleMap }) {
  const analysis = bundle.aggregates.api_surface;
  const labels = bundle.capability.labels;
  const rows = analysis.data.items.slice(0, UI_ROW_LIMIT);
  const max = Math.max(1, ...rows.map((row) => row.dependent_count));
  return (
    <div className="repo-view-body"><section className="repo-card repo-table-wrap"><table className="repo-table"><thead><tr><th>{labels.node_types.module}</th><th>{labels.metrics.dependent_count}</th><th>{labels.metrics.dependent_count}</th></tr></thead><tbody>{rows.map((row) => <tr key={row.module_id}><td><strong>{moduleName(moduleById, row.module_id)}</strong></td><td>{formatNumber(row.dependent_count)}</td><td><div className="repo-bar"><span style={{ width: `${(row.dependent_count / max) * 100}%` }} /></div></td></tr>)}</tbody></table><LocalSliceDisclosure shown={rows.length} total={analysis.data.items.length} label={bundle.capability.views.api_surface.label} labels={labels} /><BoundDisclosure page={analysis.data} labels={labels} /></section></div>
  );
}

function scoreLabel(labels: Labels, key: RepoBundle["aggregates"]["scorecard"]["fields"][number]["key"]): string {
  const keys = {
    repository_age_days: labels.metrics.repository_age,
    package_count: labels.metrics.package_count,
    module_count: labels.metrics.module_count,
    symbol_count: labels.metrics.symbol_count,
    activity_trend: labels.metrics.activity_trend,
    top_hotspots: labels.metrics.top_hotspots,
    dependency_cycle_count: labels.metrics.cycle_count,
    ownership_warnings: labels.metrics.ownership_warnings,
  } satisfies Record<typeof key, string>;
  return keys[key];
}

function scoreValue(field: RepoBundle["aggregates"]["scorecard"]["fields"][number], labels: Labels, moduleById: ModuleMap): string {
  if (field.value.status === "unavailable") return labels.unavailable;
  const value = field.value.value;
  if (value.value_kind === "count") return formatNumber(value.value);
  if (value.value_kind === "ratio") return formatPercent(value.value);
  if (value.value_kind === "module_ids") {
    return value.value.items.length === 0
      ? "0"
      : value.value.items.slice(0, 8).map((id) => moduleName(moduleById, id)).join(", ");
  }
  return value.value;
}

function ScorecardView({ bundle, moduleById }: { bundle: RepoBundle; moduleById: ModuleMap }) {
  const analysis = bundle.aggregates.scorecard;
  const labels = bundle.capability.labels;
  const fields = analysis.fields.slice(0, UI_ROW_LIMIT);
  return (
    <div className="repo-view-body">
      <div className="repo-score-grid">{fields.map((field) => {
        const value = scoreValue(field, labels, moduleById);
        const moduleIds = field.value.status === "available" && field.value.value.value_kind === "module_ids" ? field.value.value.value : null;
        return <article className="repo-score-card" key={field.key}><span>{scoreLabel(labels, field.key)}</span><div className="repo-score-value">{field.value.status === "unavailable" ? <AlertTriangle aria-hidden="true" /> : <TrendingUp aria-hidden="true" />}<strong>{value}</strong></div>{field.value.status === "unavailable" && <p>{field.value.reason}</p>}{moduleIds && <><InlineLocalSlice shown={Math.min(8, moduleIds.items.length)} total={moduleIds.items.length} labels={labels} /><InlinePageState page={moduleIds} labels={labels} /></>}<div className="repo-score-tags"><i>{field.granularity}</i><i>{field.join}</i></div></article>;
      })}</div>
      <LocalSliceDisclosure shown={fields.length} total={analysis.fields.length} label={bundle.capability.views.scorecard.label} labels={labels} />
    </div>
  );
}

const viewComponents: Record<ViewId, React.ComponentType<ViewProps>> = {
  structure_graph: StructureGraph,
  history_structure_navigation: HistoryStructure,
  dependency_topology: DependencyTopology,
  hotspot_quadrant: HotspotQuadrantView,
  hidden_coupling: HiddenCouplingView,
  structure_treemap: TreemapView,
  cadence_timeline: CadenceView,
  ownership: OwnershipView,
  api_surface: ApiSurfaceView,
  scorecard: ScorecardView,
};

function ActiveView({
  id,
  bundle,
  moduleById,
  selectedModuleId,
  onSelectModule,
  onExploreStructure,
}: ViewProps & { id: ViewId }) {
  const capability = bundle.capability.views[id];
  const labels = bundle.capability.labels;
  const ViewComponent = viewComponents[id];
  return (
    <ViewFrame capability={capability} labels={labels} allowPartial={id === "ownership"}>
      <ViewComponent
        bundle={bundle}
        moduleById={moduleById}
        selectedModuleId={selectedModuleId}
        onSelectModule={onSelectModule}
        onExploreStructure={onExploreStructure}
      />
    </ViewFrame>
  );
}

export function RepoShowcase({ bundle, analysisSource = "curated-static-fallback" }: { bundle: RepoBundle; analysisSource?: ShowcaseBundleSource }) {
  const { repository, snapshot, producer } = bundle.meta;
  const { capability } = bundle;
  const moduleById = useMemo(
    () => new Map(bundle.graph.modules.items.map((module) => [module.id, module])),
    [bundle.graph.modules.items],
  );
  const modulesBySourcePath = useMemo(() => {
    const result = new Map<string, RepoModule[]>();
    for (const moduleNode of bundle.graph.modules.items) {
      const matches = result.get(moduleNode.source_path) ?? [];
      matches.push(moduleNode);
      result.set(moduleNode.source_path, matches);
    }
    return result;
  }, [bundle.graph.modules.items]);
  const brief = useMemo(() => buildRepositoryBrief(bundle), [bundle]);
  const defaultModuleId: string | null = brief.startHere[0]?.moduleId ??
    bundle.graph.modules.items[0]?.id ?? null;
  const [selectedModuleId, setSelectedModuleId] = useState<string | null>(
    defaultModuleId,
  );
  const [unresolvedModule, setUnresolvedModule] = useState<Readonly<{
    path: string;
    reason: string;
  }> | null>(null);
  const [activeView, setActiveView] = useState<ViewId>("structure_graph");
  const [locationNotice, setLocationNotice] = useState<Readonly<{
    title: string;
    message: string;
    action: "use-current-snapshot" | "dismiss";
  }> | null>(null);
  const [navigationStatus, setNavigationStatus] = useState("");
  const [copyStatus, setCopyStatus] = useState("");
  const overviewRef = useRef<HTMLElement>(null);
  const dashboardRef = useRef<HTMLDivElement>(null);
  const copyLinkRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    function restoreLocation(announce = false) {
      const parsed = parseRepositoryLocation(new URL(window.location.href));
      const requestedPath = parsed.location.modulePath;
      let nextModuleId: string | null = defaultModuleId;
      let nextUnresolved: typeof unresolvedModule = null;
      if (requestedPath) {
        const matches = modulesBySourcePath.get(requestedPath) ?? [];
        if (matches.length === 1) {
          nextModuleId = matches[0].id;
        } else {
          nextModuleId = null;
          nextUnresolved = {
            path: requestedPath,
            reason: matches.length === 0
              ? "The requested source path is not present in this bounded snapshot."
              : "The requested source path is ambiguous in this snapshot.",
          };
        }
      }
      const nextView = parsed.location.view ?? "structure_graph";
      const messages = parsed.issues.map((issue) => issue.message);
      const staleSnapshot = Boolean(
        parsed.location.snapshotSha &&
          parsed.location.snapshotSha !== snapshot.head_sha,
      );
      if (staleSnapshot && parsed.location.snapshotSha) {
        messages.unshift(
          `The requested snapshot ${parsed.location.snapshotSha} is not loaded; this page is showing ${snapshot.head_sha}.`,
        );
      }

      setSelectedModuleId(nextModuleId);
      setUnresolvedModule(nextUnresolved);
      setActiveView(nextView);
      setCopyStatus("");
      setLocationNotice(messages.length
        ? {
            title: staleSnapshot
              ? "Investigation link needs attention"
              : "Investigation link was repaired",
            message: messages.join(" "),
            action: staleSnapshot ? "use-current-snapshot" : "dismiss",
          }
        : null);
      if (announce) {
        const moduleLabel = nextModuleId
          ? moduleById.get(nextModuleId)?.source_path ?? "repository overview"
          : requestedPath
          ? `unresolved module ${requestedPath}`
          : "repository overview";
        setNavigationStatus(
          `Restored ${capability.views[nextView].label} for ${moduleLabel}.`,
        );
      }

      const canonical = repositoryLocationUrl(
        new URL(window.location.href),
        {
          repository: repository.canonical_url,
          snapshotSha: parsed.location.snapshotSha ?? snapshot.head_sha,
          modulePath: requestedPath ??
            (nextModuleId ? moduleById.get(nextModuleId)?.source_path ?? null : null),
          view: nextView,
        },
      );
      if (canonical.href !== window.location.href) {
        window.history.replaceState(
          null,
          "",
          `${canonical.pathname}${canonical.search}${canonical.hash}`,
        );
      }
    }

    const handlePopState = () => restoreLocation(true);
    restoreLocation();
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, [
    capability.views,
    defaultModuleId,
    moduleById,
    modulesBySourcePath,
    repository.canonical_url,
    snapshot.head_sha,
  ]);

  function investigationLocation(
    moduleId: string | null,
    view: ViewId,
    missingPath: string | null = null,
  ) {
    return {
      repository: repository.canonical_url,
      snapshotSha: snapshot.head_sha,
      modulePath: moduleId
        ? moduleById.get(moduleId)?.source_path ?? null
        : missingPath,
      view,
    };
  }

  function locationFor(
    moduleId: string | null,
    view: ViewId,
    missingPath: string | null = null,
  ) {
    return repositoryLocationUrl(
      new URL(window.location.href),
      investigationLocation(moduleId, view, missingPath),
    );
  }

  function pushLocation(
    moduleId: string | null,
    view: ViewId,
    missingPath: string | null = null,
  ) {
    const next = locationFor(moduleId, view, missingPath);
    if (next.href === window.location.href) return;
    window.history.pushState(
      null,
      "",
      `${next.pathname}${next.search}${next.hash}`,
    );
  }

  function selectModule(moduleId: string) {
    if (!moduleById.has(moduleId)) return;
    pushLocation(moduleId, activeView);
    setNavigationStatus("");
    setSelectedModuleId(moduleId);
    setUnresolvedModule(null);
    setLocationNotice(null);
    setCopyStatus("");
  }

  function selectView(view: ViewId) {
    pushLocation(selectedModuleId, view, unresolvedModule?.path ?? null);
    setNavigationStatus("");
    setActiveView(view);
    setLocationNotice(null);
    setCopyStatus("");
  }

  function dismissLocationNotice() {
    setLocationNotice(null);
    queueMicrotask(() => copyLinkRef.current?.focus());
  }

  function recoverModule() {
    if (defaultModuleId) selectModule(defaultModuleId);
  }

  function normalizeCurrentLocation() {
    const next = locationFor(
      selectedModuleId,
      activeView,
      unresolvedModule?.path ?? null,
    );
    window.history.replaceState(
      null,
      "",
      `${next.pathname}${next.search}${next.hash}`,
    );
    setLocationNotice(null);
    setCopyStatus("");
    queueMicrotask(() => copyLinkRef.current?.focus());
  }

  async function copyInvestigationLink() {
    try {
      if (!navigator.clipboard?.writeText) {
        throw new Error("Clipboard access is unavailable");
      }
      const location = investigationLocation(
        selectedModuleId,
        activeView,
        unresolvedModule?.path ?? null,
      );
      const current = repositoryLocationUrl(
        new URL(window.location.href),
        location,
      );
      // The copied link is the share form: investigation parameters only,
      // no foreign query parameters and no fragment.
      const share = investigationShareUrl(
        new URL(window.location.href),
        location,
      );
      const sourceHref = window.location.href;
      await navigator.clipboard.writeText(share.href);
      if (window.location.href === sourceHref && current.href !== sourceHref) {
        window.history.replaceState(
          null,
          "",
          `${current.pathname}${current.search}${current.hash}`,
        );
        setLocationNotice(null);
      }
      setCopyStatus("Investigation link copied.");
    } catch {
      setCopyStatus("Investigation link could not be copied.");
    }
  }

  function openAnalysis(view: ViewId) {
    selectView(view);
    const dashboard = dashboardRef.current;
    if (!dashboard) return;
    dashboard.focus({ preventScroll: true });
    const reduceMotion = window.matchMedia?.(
      "(prefers-reduced-motion: reduce)",
    ).matches ?? false;
    dashboard.scrollIntoView?.({
      behavior: reduceMotion ? "auto" : "smooth",
      block: "start",
    });
  }

  function openModuleFromPalette(moduleId: string) {
    selectModule(moduleId);
    const inspector = overviewRef.current?.querySelector<HTMLElement>(
      "[data-module-inspector]",
    );
    if (!inspector) return;
    inspector.focus({ preventScroll: true });
    const reduceMotion = window.matchMedia?.(
      "(prefers-reduced-motion: reduce)",
    ).matches ?? false;
    inspector.scrollIntoView?.({
      behavior: reduceMotion ? "auto" : "smooth",
      block: "start",
    });
  }
  return (
    <article ref={overviewRef} className="repo-overview" data-head-sha={snapshot.head_sha} data-analysis-source={analysisSource}>
      <header className="repo-overview-heading">
        <div className="repo-identity"><span className="repo-avatar"><Package aria-hidden="true" /></span><div><span>{repository.host} · {availabilityText(repository.default_branch, capability.labels)}</span><strong>{repository.owner}/{repository.name}</strong></div></div>
        <div className="repo-meta-row"><span><GitCommitHorizontal aria-hidden="true" /><code>{shortSha(snapshot.head_sha)}</code></span><span><Clock3 aria-hidden="true" />{formatDate(snapshot.ingested_at)}</span><span><Code2 aria-hidden="true" />{producer.exporter}</span><span><Database aria-hidden="true" />{analysisSource === "khive-db-snapshot" ? "khive DB snapshot" : "curated static fallback"}</span><RepositoryCommandPalette bundle={bundle} activeView={activeView} selectedModuleId={selectedModuleId} onSelectModule={openModuleFromPalette} onSelectView={openAnalysis} onCopyLink={copyInvestigationLink} /><button ref={copyLinkRef} type="button" className="repo-copy-link" onClick={copyInvestigationLink}><Copy aria-hidden="true" /> Copy investigation link</button>{copyStatus && <span role="status" className="repo-copy-status">{copyStatus}</span>}</div>
      </header>
      <section className="repo-capability-strip" aria-label={capability.labels.product}>
        <div><ShieldCheck aria-hidden="true" /><div><strong>{capability.labels.product}</strong><span>{capability.mode}</span></div></div>
        <div className="repo-capability-flags">{Object.values(capability.languages).map((language) => <i key={language.label}>{language.label} · {language.module_join ? capability.views.history_structure_navigation.label : capability.labels.unavailable}</i>)}</div>
      </section>
      <span
        className="visually-hidden"
        role="status"
        aria-live="polite"
        aria-label="Investigation navigation"
      >
        {navigationStatus}
      </span>
      {locationNotice && (
        <aside
          className="repo-investigation-notice"
          role="status"
          aria-label="Investigation link status"
        >
          <AlertTriangle aria-hidden="true" />
          <span><strong>{locationNotice.title}</strong>{locationNotice.message}</span>
          {locationNotice.action === "use-current-snapshot"
            ? <button type="button" onClick={normalizeCurrentLocation}>Use current snapshot</button>
            : <button type="button" onClick={dismissLocationNotice}>Dismiss</button>}
        </aside>
      )}
      <RepositoryTriage
        key={snapshot.head_sha}
        bundle={bundle}
        selectedModuleId={selectedModuleId}
        unresolvedModule={unresolvedModule}
        onSelectModule={selectModule}
        onRecoverModule={recoverModule}
        canRecoverModule={defaultModuleId !== null}
        onOpenAnalysis={openAnalysis}
      />
      <div
        className="repo-dashboard"
        data-repository-dashboard
        id="repository-analysis-dashboard"
        ref={dashboardRef}
        role="region"
        aria-label={`${capability.labels.product} analysis`}
        tabIndex={-1}
      >
        <nav className="repo-view-nav" aria-label={capability.labels.product}>
          <span>{capability.labels.product}</span>
          {viewOrder.map((id) => {
            const view = capability.views[id];
            const Icon = viewIcons[id];
            return <button type="button" data-view-id={id} className={activeView === id ? "active" : ""} aria-current={activeView === id ? "page" : undefined} key={id} onClick={() => selectView(id)}><Icon aria-hidden="true" /><span>{view.label}</span><i className={view.status} aria-hidden="true" />{view.status === "unavailable" && <span className="visually-hidden">{capability.labels.unavailable}</span>}</button>;
          })}
        </nav>
        <section className="repo-view-panel" aria-label={capability.views[activeView].label}>
          <ActiveView key={`${snapshot.head_sha}-${activeView}`} id={activeView} bundle={bundle} moduleById={moduleById} selectedModuleId={selectedModuleId} onSelectModule={selectModule} onExploreStructure={() => selectView("structure_graph")} />
        </section>
      </div>
    </article>
  );
}
