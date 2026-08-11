"use client";

import {
  ArrowRight,
  Boxes,
  Braces,
  CheckCircle2,
  CircleHelp,
  Code2,
  Eye,
  FileText,
  GitCommitHorizontal,
  GitFork,
  Network,
  Package,
  Radar,
  Search,
  Signpost,
  Users,
} from "@/icons";
import { useMemo, useState } from "react";

import { DataState } from "@/components/data-state";
import {
  buildModuleInsight,
  buildRepositoryBrief,
  findRepositoryModules,
  type RepositoryMetric,
  type RepositorySignal,
} from "@/lib/repository-brief";
import type { RepoBundle, RepoModule, ViewId } from "@/lib/repo-bundle";

import styles from "./repository-triage.module.css";

export type RepositoryTriageProps = Readonly<{
  bundle: RepoBundle;
  onOpenAnalysis: (view: ViewId) => void;
}>;

function formatNumber(value: number): string {
  return new Intl.NumberFormat("en").format(value);
}

function formatPercent(value: number): string {
  return new Intl.NumberFormat("en", {
    style: "percent",
    maximumFractionDigits: 0,
  }).format(value);
}

function shortSha(value: string): string {
  return value.slice(0, 8);
}

function MetricValue({
  metric,
}: {
  metric: RepositoryMetric;
}) {
  return (
    <dd>
      <strong>{formatNumber(metric.shown)}</strong>
      <small className={styles.metricCoverage}>{metric.summary}</small>
    </dd>
  );
}

function signalIcon(kind: RepositorySignal["kind"]) {
  if (kind === "hotspot") return <Radar aria-hidden="true" />;
  if (kind === "dependency_cycle") return <GitFork aria-hidden="true" />;
  if (kind === "hidden_coupling") return <Braces aria-hidden="true" />;
  return <Users aria-hidden="true" />;
}

function Classification({
  value,
}: {
  value: RepositorySignal["classification"];
}) {
  return (
    <span
      className={`${styles.classification} ${styles[value]}`}
      data-classification={value}
    >
      {value === "observed"
        ? <CheckCircle2 aria-hidden="true" />
        : <CircleHelp aria-hidden="true" />}
      {value === "observed" ? "Observed" : "Candidate"}
    </span>
  );
}

function ModuleButton({
  module,
  selected,
  detail,
  onSelect,
}: {
  module: RepoModule;
  selected: boolean;
  detail: string;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      className={styles.moduleButton}
      data-module-id={module.id}
      aria-label={`Inspect ${module.source_path}`}
      aria-pressed={selected}
      onClick={onSelect}
    >
      <span className={styles.moduleIcon}>
        <Code2 aria-hidden="true" />
      </span>
      <span className={styles.moduleCopy}>
        <strong>{module.source_path}</strong>
        <span>{detail}</span>
      </span>
      <ArrowRight aria-hidden="true" />
    </button>
  );
}

export function RepositoryTriage({
  bundle,
  onOpenAnalysis,
}: RepositoryTriageProps) {
  const brief = useMemo(() => buildRepositoryBrief(bundle), [bundle]);
  const labels = bundle.capability.labels;
  const moduleLabel = labels.node_types.module;
  const moduleLabelLower = moduleLabel.toLocaleLowerCase();
  const moduleById = useMemo(
    () =>
      new Map(bundle.graph.modules.items.map((module) => [module.id, module])),
    [bundle.graph.modules.items],
  );
  const initialModuleId = brief.startHere[0]?.moduleId ??
    bundle.graph.modules.items[0]?.id ?? null;
  const [selectedModuleId, setSelectedModuleId] = useState<string | null>(
    initialModuleId,
  );
  const [query, setQuery] = useState("");
  const selectedInsight = useMemo(
    () =>
      selectedModuleId ? buildModuleInsight(bundle, selectedModuleId) : null,
    [bundle, selectedModuleId],
  );
  const searchResults = useMemo(
    () => (query.trim() ? findRepositoryModules(bundle, query, 8) : []),
    [bundle, query],
  );

  function selectModule(moduleId: string) {
    setSelectedModuleId(moduleId);
  }

  function selectSignal(signal: RepositorySignal) {
    const moduleId = signal.moduleIds.find((candidate) =>
      moduleById.has(candidate)
    );
    if (moduleId) selectModule(moduleId);
  }

  return (
    <section
      className={styles.root}
      aria-label="Repository triage"
      data-repository-triage
    >
      <header className={styles.header}>
        <div className={styles.heading}>
          <span className={styles.eyebrow}>
            <Signpost aria-hidden="true" /> Repository triage
          </span>
          <h2>What deserves attention?</h2>
          <p>
            Start with architectural leverage, inspect evidence, then open the
            underlying analysis. Signals describe this captured snapshot; they
            are not automatic defect claims.
          </p>
        </div>
        <label className={styles.search}>
          <span>Find a {moduleLabelLower} or path</span>
          <span className={styles.searchControl}>
            <Search aria-hidden="true" />
            <input
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="pool.rs, writer_task, server…"
              aria-label={`Find a ${moduleLabelLower} or path`}
            />
          </span>
        </label>
      </header>

      <dl className={styles.metrics} aria-label="Captured repository metrics">
        <div data-repository-metric="packages">
          <dt>
            <Package aria-hidden="true" /> {labels.metrics.package_count}
          </dt>
          <MetricValue metric={brief.metrics.packages} />
        </div>
        <div data-repository-metric="modules">
          <dt>
            <Boxes aria-hidden="true" /> {labels.metrics.module_count}
          </dt>
          <MetricValue metric={brief.metrics.modules} />
        </div>
        <div data-repository-metric="commits">
          <dt>
            <GitCommitHorizontal aria-hidden="true" /> {labels.metrics.commits}
          </dt>
          <MetricValue metric={brief.metrics.commits} />
        </div>
        <div data-repository-metric="cycles">
          <dt>
            <GitFork aria-hidden="true" /> {labels.metrics.cycle_count}
          </dt>
          <MetricValue metric={brief.metrics.cycles} />
        </div>
      </dl>

      <div className={styles.layout}>
        <div className={styles.content}>
          <section className={styles.panel} aria-labelledby="repo-start-here">
            <div className={styles.panelHeading}>
              <div>
                <span>Question 1</span>
                <h3 id="repo-start-here">Where should I start?</h3>
                <p>
                  These captured {moduleLabelLower}{" "}
                  records have the widest dependent surface in this snapshot.
                </p>
              </div>
              <Network aria-hidden="true" />
            </div>

            {query.trim()
              ? (
                <div
                  className={styles.searchResults}
                  aria-label={`${moduleLabel} search results`}
                >
                  <div className={styles.resultHeading}>
                    <strong>{searchResults.length} captured matches</strong>
                    {searchResults.length > 0 && (
                      <button
                        type="button"
                        onClick={() => setQuery("")}
                      >
                        Clear search
                      </button>
                    )}
                  </div>
                  {searchResults.length
                    ? (
                      searchResults.map((module) => (
                        <ModuleButton
                          key={module.id}
                          module={module}
                          selected={module.id === selectedModuleId}
                          detail={`${module.language} · ${module.module_path}`}
                          onSelect={() => selectModule(module.id)}
                        />
                      ))
                    )
                    : (
                      <DataState
                        className={styles.empty}
                        state="empty"
                        title={`No captured ${moduleLabelLower} matches that path`}
                        message={`Try a filename, crate, or ${moduleLabelLower} segment from this snapshot.`}
                        action={{
                          label: "Clear search",
                          onClick: () => setQuery(""),
                        }}
                      />
                    )}
                </div>
              )
              : (
                <div className={styles.startList}>
                  {brief.startHere.length
                    ? (
                      brief.startHere.map((entry, index) => {
                        const moduleNode = moduleById.get(entry.moduleId);
                        if (!moduleNode) return null;
                        return (
                          <div className={styles.startRow} key={entry.moduleId}>
                            <span className={styles.rank}>
                              {String(index + 1).padStart(2, "0")}
                            </span>
                            <ModuleButton
                              module={moduleNode}
                              selected={moduleNode.id === selectedModuleId}
                              detail={`${bundle.capability.views.api_surface.label} · ${
                                formatNumber(entry.dependentCount)
                              } ${labels.metrics.dependent_count} · ${entry.modulePath}`}
                              onSelect={() =>
                                selectModule(moduleNode.id)}
                            />
                          </div>
                        );
                      })
                    )
                    : (
                      <DataState
                        className={styles.empty}
                        state="empty"
                        title="No API-surface recommendation is supported"
                        message={`No captured ${moduleLabelLower} has a non-zero ${labels.metrics.dependent_count.toLocaleLowerCase()} in this snapshot.`}
                        action={{
                          label:
                            `Open ${bundle.capability.views.api_surface.label}`,
                          onClick: () => onOpenAnalysis("api_surface"),
                        }}
                      />
                    )}
                </div>
              )}
          </section>

          <section
            className={styles.panel}
            aria-labelledby="repo-attention-signals"
          >
            <div className={styles.panelHeading}>
              <div>
                <span>Question 2</span>
                <h3 id="repo-attention-signals">
                  What should I verify before changing code?
                </h3>
                <p>
                  History and topology surface evidence-backed signals for human
                  review.
                </p>
              </div>
              <Eye aria-hidden="true" />
            </div>
            <div className={styles.signalGrid}>
              {brief.attentionSignals.length
                ? brief.attentionSignals.map((signal) => (
                  <article
                    className={styles.signal}
                    data-signal-kind={signal.kind}
                    key={signal.id}
                  >
                    <div className={styles.signalTop}>
                      <span className={styles.signalIcon}>
                        {signalIcon(signal.kind)}
                      </span>
                      <Classification value={signal.classification} />
                    </div>
                    <h4>{signal.title}</h4>
                    <p>{signal.summary}</p>
                    <strong className={styles.why}>
                      {signal.whyItMatters}
                    </strong>
                    <dl className={styles.evidenceCompact}>
                      {signal.evidence.map((item) => (
                        <div key={`${signal.id}-${item.label}`}>
                          <dt>{item.label}</dt>
                          <dd>{item.value}</dd>
                        </div>
                      ))}
                    </dl>
                    <div className={styles.signalActions}>
                      {signal.moduleIds.some((moduleId) =>
                        moduleById.has(moduleId)
                      ) && (
                        <button
                          type="button"
                          onClick={() => selectSignal(signal)}
                        >
                          Inspect {moduleLabelLower}
                        </button>
                      )}
                      <button
                        type="button"
                        aria-controls="repository-analysis-dashboard"
                        aria-label={`Open full analysis: ${
                          bundle.capability.views[signal.targetView].label
                        }`}
                        onClick={() => onOpenAnalysis(signal.targetView)}
                      >
                        Open {bundle.capability.views[signal.targetView].label}
                        {" "}
                        <ArrowRight aria-hidden="true" />
                      </button>
                    </div>
                  </article>
                ))
                : (
                  <DataState
                    className={styles.empty}
                    state="empty"
                    title="No attention signal is supported"
                    message="This snapshot does not contain enough measured evidence for a triage recommendation."
                    action={{
                      label: `Open ${bundle.capability.views.scorecard.label}`,
                      onClick: () => onOpenAnalysis("scorecard"),
                    }}
                  />
                )}
            </div>
          </section>
        </div>

        <aside
          className={styles.inspector}
          aria-label={`${moduleLabel} evidence`}
          data-module-inspector
          aria-live="polite"
        >
          {selectedInsight
            ? (
              <>
                <header className={styles.inspectorHeader}>
                  <span>
                    <FileText aria-hidden="true" /> {moduleLabel} evidence
                  </span>
                  <h3>{selectedInsight.module.source_path}</h3>
                  <p>
                    {selectedInsight.module.language} ·{" "}
                    {selectedInsight.module.module_path}
                  </p>
                </header>

                <dl className={styles.inspectorMetrics}>
                  <div>
                    <dt>{labels.metrics.fan_in}</dt>
                    <dd>{formatNumber(selectedInsight.topology.fanIn)}</dd>
                  </div>
                  <div>
                    <dt>{labels.metrics.fan_out}</dt>
                    <dd>{formatNumber(selectedInsight.topology.fanOut)}</dd>
                  </div>
                  <div>
                    <dt>{labels.metrics.commits}</dt>
                    <dd>
                      {formatNumber(selectedInsight.recentCommits.length)}
                    </dd>
                  </div>
                  <div>
                    <dt>{labels.metrics.bus_factor}</dt>
                    <dd>{selectedInsight.ownership?.busFactor ?? "—"}</dd>
                  </div>
                </dl>

                {selectedInsight.hotspot && (
                  <div className={styles.inspectorSignal}>
                    <Classification value="candidate" />
                    <strong>
                      {bundle.capability.views.hotspot_quadrant.label} candidate
                    </strong>
                    <span>
                      {selectedInsight.hotspot.commitCount} captured{" "}
                      {labels.metrics.commits} · {labels.metrics.fan_in}{" "}
                      {selectedInsight.hotspot.fanIn} · {labels
                        .hotspot_quadrants[selectedInsight.hotspot.quadrant]}
                    </span>
                  </div>
                )}

                <div className={styles.relationships}>
                  <section>
                    <h4>{labels.metrics.fan_in}</h4>
                    {selectedInsight.dependents.length
                      ? (
                        <ul>
                          {selectedInsight.dependents.slice(0, 6).map((
                            module,
                          ) => <li key={module.id}>{module.source_path}</li>)}
                        </ul>
                      )
                      : <p>No captured {labels.metrics.fan_in} evidence.</p>}
                  </section>
                  <section>
                    <h4>{labels.metrics.fan_out}</h4>
                    {selectedInsight.dependencies.length
                      ? (
                        <ul>
                          {selectedInsight.dependencies
                            .slice(0, 6)
                            .map((module) => (
                              <li key={module.id}>{module.source_path}</li>
                            ))}
                        </ul>
                      )
                      : <p>No captured {labels.metrics.fan_out} evidence.</p>}
                  </section>
                </div>

                {selectedInsight.couplings.length > 0
                  ? (
                    <section className={styles.inspectorSection}>
                      <h4>{bundle.capability.views.hidden_coupling.label}</h4>
                      <ul>
                        {selectedInsight.couplings.slice(0, 4).map((
                          coupling,
                        ) => (
                          <li key={coupling.module.id}>
                            <span>{coupling.module.source_path}</span>
                            <strong>
                              {coupling.cochangeCount}{" "}
                              {labels.metrics.cochange_count} ·{" "}
                              {formatPercent(coupling.support)}{" "}
                              {labels.metrics.support}
                            </strong>
                          </li>
                        ))}
                      </ul>
                    </section>
                  )
                  : (
                    <section className={styles.inspectorSection}>
                      <h4>{bundle.capability.views.hidden_coupling.label}</h4>
                      <p>
                        No pair for this {moduleLabelLower}{" "}
                        appears in the captured top-N. Inspect the coverage
                        evidence below before treating this as absence.
                      </p>
                    </section>
                  )}

                <section className={styles.inspectorSection}>
                  <h4>{labels.metrics.commits}</h4>
                  {selectedInsight.recentCommits.length
                    ? (
                      <ul>
                        {selectedInsight.recentCommits.slice(0, 5).map((
                          commit,
                        ) => (
                          <li key={commit.id}>
                            <code>{shortSha(commit.sha)}</code>
                            <span>{commit.subject}</span>
                          </li>
                        ))}
                      </ul>
                    )
                    : (
                      <p>
                        No captured {labels.metrics.commits.toLocaleLowerCase()}
                        {" "}
                        for this {moduleLabelLower}.
                      </p>
                    )}
                </section>

                <section className={styles.inspectorSection}>
                  <h4>Evidence</h4>
                  <dl className={styles.evidence}>
                    {selectedInsight.evidence.map((item) => (
                      <div key={item.label}>
                        <dt>{item.label}</dt>
                        <dd>{item.value}</dd>
                        {item.detail && (
                          <dd className={styles.evidenceDetail}>
                            {item.detail}
                          </dd>
                        )}
                      </div>
                    ))}
                  </dl>
                </section>
              </>
            )
            : (
              <DataState
                className={styles.empty}
                state="unavailable"
                title={`${moduleLabel} evidence is unavailable`}
                message={`This snapshot does not contain a captured ${moduleLabelLower} to inspect.`}
              />
            )}
        </aside>
      </div>

      <footer className={styles.provenance}>
        <span>
          Snapshot <code>{shortSha(bundle.meta.snapshot.head_sha)}</code>
        </span>
        <span>
          Generated{" "}
          {new Date(bundle.meta.snapshot.ingested_at).toLocaleString("en")}
        </span>
        <span>
          Evidence is bounded by each analysis&apos;s declared window and export
          limits.
        </span>
      </footer>
    </section>
  );
}
