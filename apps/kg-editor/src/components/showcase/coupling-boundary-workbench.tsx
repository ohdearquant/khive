"use client";

import { useId } from "react";

import type {
  CouplingComparisonResult,
  CouplingCoverageStatus,
  CouplingEndpointEvidence,
  CouplingEvidenceBoundary,
  CouplingEvidenceState,
} from "@/lib/coupling-comparison";
import {
  couplingAnalysisWindowLabel,
  couplingComparisonResultStatus,
} from "@/lib/coupling-comparison";

function formatPercent(value: number): string {
  return new Intl.NumberFormat("en", {
    style: "percent",
    maximumFractionDigits: 1,
  }).format(value);
}

function formatRatio(value: number | null): string {
  return value == null ? "unavailable" : formatPercent(value);
}

function stateLabel(state: CouplingEvidenceState): string {
  return state[0].toLocaleUpperCase() + state.slice(1);
}

function statusBoundary(
  label: string,
  boundary: CouplingEvidenceBoundary,
): string {
  return `${label}: ${boundary.shown} shown of ${boundary.declared ?? "unknown"} declared · ${boundary.status}`;
}

function Boundary({
  value,
  label,
}: Readonly<{
  value: CouplingEvidenceBoundary;
  label: string;
}>) {
  return (
    <span
      className={`repo-boundary-evidence-coverage ${value.status}`}
      data-coverage-status={value.status}
    >
      <span>{label}: {value.shown} / {value.declared ?? "unknown"} shown · bound {value.bound}</span>
      {value.reason && <em>{value.reason}</em>}
    </span>
  );
}

function EvidenceState({
  state,
  status,
}: Readonly<{
  state: CouplingEvidenceState;
  status: CouplingCoverageStatus;
}>) {
  return (
    <span
      className={`repo-boundary-evidence-state ${state}`}
      data-evidence-state={state}
      data-coverage-status={status}
    >
      {stateLabel(state)}
    </span>
  );
}

function EndpointArticle({
  endpoint,
  selected,
  onInspectModule,
}: Readonly<{
  endpoint: CouplingEndpointEvidence;
  selected: boolean;
  onInspectModule: (moduleId: string) => void;
}>) {
  const { module: moduleNode } = endpoint;
  return (
    <article
      className="repo-boundary-endpoint"
      aria-label={`Boundary endpoint ${moduleNode.source_path}`}
    >
      <header>
        <div>
          <span>Endpoint evidence</span>
          <strong>{moduleNode.module_path}</strong>
          <code>{moduleNode.source_path}</code>
        </div>
        <button
          type="button"
          aria-controls="repository-module-inspector"
          aria-expanded={selected}
          aria-label={`Show ${moduleNode.source_path} in module inspector`}
          onClick={() => onInspectModule(moduleNode.id)}
        >
          {selected ? "Shown in inspector" : "Show in inspector"}
        </button>
      </header>
      <dl>
        <div>
          <dt>Topology <EvidenceState state={endpoint.topology.state} status={endpoint.topology.boundary.status} /></dt>
          <dd>
            {endpoint.topology.state === "present"
              ? <span>Fan-in {endpoint.topology.fanIn} · Fan-out {endpoint.topology.fanOut}</span>
              : <span>{stateLabel(endpoint.topology.state)} topology evidence</span>}
            <Boundary value={endpoint.topology.boundary} label="Topology rows" />
          </dd>
        </div>
        <div>
          <dt>SCC membership <EvidenceState state={endpoint.scc.state} status={endpoint.scc.boundary.status} /></dt>
          <dd>
            {endpoint.scc.state === "present"
              ? (
                <ul>
                  {endpoint.scc.items.map((cycle) => (
                    <li key={cycle.id}>
                      <code>{cycle.id}</code> · {cycle.modules.map((member) => member.source_path).join(" · ")}
                      <Boundary value={cycle.memberBoundary} label="SCC members" />
                    </li>
                  ))}
                </ul>
              )
              : endpoint.scc.state === "absent"
              ? <span>No captured SCC membership under complete evidence coverage.</span>
              : <span>SCC membership is unknown under the captured coverage.</span>}
            <Boundary value={endpoint.scc.boundary} label="SCC rows" />
          </dd>
        </div>
        <div>
          <dt>History <EvidenceState state={endpoint.history.state} status={endpoint.history.boundary.status} /></dt>
          <dd>
            <Boundary value={endpoint.history.boundary} label="Commit IDs" />
          </dd>
        </div>
        <div>
          <dt>Hotspot row <EvidenceState state={endpoint.hotspot.state} status={endpoint.hotspot.boundary.status} /></dt>
          <dd>
            {endpoint.hotspot.state === "present"
              ? (
                <span>
                  {endpoint.hotspot.commitCount} captured commits · fan-in {endpoint.hotspot.fanIn} · <code>{endpoint.hotspot.quadrant}</code>{endpoint.hotspot.rank == null ? "" : ` · captured rank ${endpoint.hotspot.rank}`}
                </span>
              )
              : <span>{stateLabel(endpoint.hotspot.state)} hotspot evidence</span>}
            <span className="repo-boundary-window">
              Hotspot window: {couplingAnalysisWindowLabel(endpoint.hotspot.window)}
            </span>
            <Boundary value={endpoint.hotspot.boundary} label="Hotspot rows" />
          </dd>
        </div>
        <div>
          <dt>Ownership rows <EvidenceState state={endpoint.ownership.state} status={endpoint.ownership.boundary.status} /></dt>
          <dd>
            {endpoint.ownership.state === "present" && (
              <>
                <span>
                  {endpoint.ownership.commitCount} captured commits · author concentration {formatRatio(endpoint.ownership.authorConcentration)} · bus factor {endpoint.ownership.busFactor ?? "unavailable"}
                </span>
                {endpoint.ownership.authors.length > 0 && (
                  <ul>
                    {endpoint.ownership.authors.map((author) => (
                      <li key={author.author}>{author.author} · {author.commits} commits · {formatPercent(author.share)}</li>
                    ))}
                  </ul>
                )}
              </>
            )}
            {endpoint.ownership.state !== "present" && (
              <span>{stateLabel(endpoint.ownership.state)} ownership evidence</span>
            )}
            <span className="repo-boundary-window">
              Ownership window: {couplingAnalysisWindowLabel(endpoint.ownership.window)}
            </span>
            <Boundary value={endpoint.ownership.boundary} label="Author rows" />
            <p>{endpoint.ownership.caveat}</p>
          </dd>
        </div>
      </dl>
    </article>
  );
}

export function CouplingBoundaryWorkbench({
  result,
  selectedModuleId,
  onInspectModule,
}: Readonly<{
  result: CouplingComparisonResult;
  selectedModuleId: string | null;
  onInspectModule: (moduleId: string) => void;
}>) {
  const headingId = useId();
  if (result.status === "unavailable") {
    return (
      <section
        className="repo-boundary-workbench unavailable"
        role="region"
        aria-labelledby={headingId}
      >
        <header>
          <div>
            <span>Focused pair evidence</span>
            <h3 id={headingId}>Boundary evidence workbench</h3>
          </div>
        </header>
        <p role="status" aria-label="Boundary evidence status">
          {couplingComparisonResultStatus(result)}
        </p>
      </section>
    );
  }

  const comparison = result.value;
  const directMessage = comparison.directDependency.state === "present"
    ? `Captured direct dependency: ${comparison.directDependency.directions.map((direction) => direction.replaceAll("_", " ")).join(" · ")}`
    : comparison.directDependency.state === "absent"
    ? "No captured direct dependency edge under complete structure evidence."
    : "Direct dependency unknown under incomplete structure evidence.";

  return (
    <section
      className="repo-boundary-workbench"
      role="region"
      aria-labelledby={headingId}
    >
      <header>
        <div>
          <span>Focused pair evidence</span>
          <h3 id={headingId}>Boundary evidence workbench</h3>
        </div>
        <p>{comparison.caveat}</p>
      </header>
      <p
        className="repo-boundary-status"
        role="status"
        aria-label="Boundary evidence status"
      >
        {couplingComparisonResultStatus(result)} · {statusBoundary("Shared commits", comparison.sharedCommits.boundary)} · {statusBoundary("Common structural neighbors", comparison.commonNeighbors.boundary)} · direct dependency {comparison.directDependency.state}
      </p>

      <div className="repo-boundary-endpoints">
        {comparison.endpoints.map((endpoint) => (
          <EndpointArticle
            key={endpoint.module.id}
            endpoint={endpoint}
            selected={selectedModuleId === endpoint.module.id}
            onInspectModule={onInspectModule}
          />
        ))}
      </div>

      <div className="repo-boundary-shared">
        <section aria-label="Shared commit evidence">
          <h4>Shared commits <EvidenceState state={comparison.sharedCommits.state} status={comparison.sharedCommits.boundary.status} /></h4>
          <Boundary value={comparison.sharedCommits.boundary} label="Shared commits" />
          {comparison.sharedCommits.items.length > 0 && (
            <ol>
              {comparison.sharedCommits.items.map((item) => (
                <li key={item.id}>
                  <code>{item.commit.short_sha}</code>
                  <span>{item.commit.subject}</span>
                </li>
              ))}
            </ol>
          )}
        </section>
        <section aria-label="Common structural neighbor evidence">
          <h4>Common structural neighbors <EvidenceState state={comparison.commonNeighbors.state} status={comparison.commonNeighbors.boundary.status} /></h4>
          <Boundary value={comparison.commonNeighbors.boundary} label="Common neighbors" />
          {comparison.commonNeighbors.items.length > 0 && (
            <ul>
              {comparison.commonNeighbors.items.map((item) => (
                <li key={`${item.module.id}-${item.leftRelation}-${item.leftDirection}-${item.rightRelation}-${item.rightDirection}`}>
                  <code>{item.module.source_path}</code>
                  <span>left {item.leftDirection} {item.leftRelation} · right {item.rightDirection} {item.rightRelation}</span>
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>

      <section className="repo-boundary-direct" aria-label="Direct dependency evidence">
        <h4>Direct dependency <EvidenceState state={comparison.directDependency.state} status={comparison.directDependency.boundary.status} /></h4>
        <p>{directMessage}</p>
        <Boundary value={comparison.directDependency.boundary} label="Direct edges" />
      </section>

      <section className="repo-boundary-prompts" aria-label="Boundary verification prompts">
        <h4>Verify next</h4>
        <ol>
          {comparison.verifyPrompts.map((prompt) => <li key={prompt}>{prompt}</li>)}
        </ol>
      </section>
    </section>
  );
}
