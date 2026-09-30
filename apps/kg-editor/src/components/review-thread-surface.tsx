"use client";

import { useMemo, type FormEvent } from "react";

import { DataState } from "@/components/data-state";
import { EntityKindMark, NoteKindMark, RelationMark } from "@/components/ontology-mark";
import type { ReviewInput } from "@/lib/review-bundle";
import {
  buildReviewThreadModel,
  type ReviewAnnotation,
  type ReviewThreadEvent,
  type ReviewThreadUnit,
} from "@/lib/review-thread";

export type ReviewThreadSurfaceProps = {
  input: ReviewInput;
  selectedUnitKey: string;
  onSelectUnit: (key: string) => void;
  annotations: readonly ReviewAnnotation[];
  query?: string;
  onQuery?: (value: string) => void;
  onImport?: () => void;
  draft?: string;
  onDraft?: (value: string) => void;
  onAddLocalNote?: () => void;
};

function rowChange(unit: ReviewThreadUnit): "added" | "modified" | "removed" {
  if (unit.change) return unit.change.change;
  const operation = unit.operations[0];
  if (operation?.op === "delete") return "removed";
  if (operation?.op === "create" || operation?.op === "link") return "added";
  return "modified";
}

function matchesQuery(unit: ReviewThreadUnit, query: string): boolean {
  if (!query) return true;
  return [
    unit.id,
    unit.substrate,
    unit.title,
    unit.subtitle,
    unit.tier,
    ...unit.operations.map((operation) => operation.summary),
  ].some((value) => value.toLocaleLowerCase().includes(query));
}

function unitOntologyValue(unit: ReviewThreadUnit): string | null {
  const paths = unit.substrate === "entity"
    ? ["entity_kind", "kind"]
    : unit.substrate === "note" ? ["note_kind", "kind"] : ["relation"];
  for (const path of paths) {
    for (const operation of unit.operations) {
      const value = operation.after?.[path] ?? operation.before?.[path];
      if (typeof value === "string" && value.trim()) return value;
    }
    const field = unit.change?.fields.find((item) => item.path === path);
    const value = field?.after ?? field?.before;
    if (typeof value === "string" && value.trim()) return value;
  }
  return null;
}

function UnitOntologyMark({ unit }: { unit: ReviewThreadUnit }) {
  const value = unitOntologyValue(unit);
  if (value === null) return null;
  if (unit.substrate === "entity") return <EntityKindMark kind={value} />;
  if (unit.substrate === "note") return <NoteKindMark kind={value} />;
  return <RelationMark relation={value} />;
}

function EmptyUnits({
  query,
  onQuery,
  onImport,
}: {
  query: string;
  onQuery?: (value: string) => void;
  onImport?: () => void;
}) {
  const title = query ? `No reviewable units match “${query}”` : "No reviewable units in this report";
  const message = query
    ? "The current filter matches no entity, note, or edge in the loaded review."
    : "This review contains no change-set operation or semantic change to open.";
  const action = query && onQuery
    ? { label: "Clear filter", onClick: () => onQuery("") }
    : onImport ? { label: "Import another review bundle", onClick: onImport } : null;
  if (action) return <DataState state="empty" title={title} message={message} action={action} />;
  return <section className="empty-state" data-state="empty" role="status"><strong>{title}</strong><p>{message}</p></section>;
}

function UnitRow({
  unit,
  selected,
  onSelect,
}: {
  unit: ReviewThreadUnit;
  selected: boolean;
  onSelect: () => void;
}) {
  const change = rowChange(unit);
  return (
    <article className={`change-card review-thread-unit ${change} ${selected ? "selected" : ""}`}>
      <button
        type="button"
        className="change-summary review-thread-unit-row"
        data-review-unit-row
        data-keyboard-row
        data-unit-key={unit.key}
        aria-keyshortcuts="J K Enter"
        aria-pressed={selected}
        onClick={onSelect}
      >
        <span className="change-sign" aria-hidden="true">
          {change === "added" ? "+" : change === "removed" ? "−" : "~"}
        </span>
        <span className="change-title">
          <strong>{unit.title}</strong>
          <span>{unit.subtitle}</span>
          <code className="record-id review-thread-unit-id">{unit.id}</code>
        </span>
        <span className={`substrate-chip ${unit.substrate === "edge" ? "edge" : ""}`}>{unit.substrate}</span>
        <span className={`tier-pill ${unit.tier}`}>Tier {unit.tier === "tier_2" ? "2" : "1"}</span>
        <span aria-hidden="true">›</span>
      </button>
    </article>
  );
}

function eventTime(event: ReviewThreadEvent) {
  if (event.occurredAt === null) {
    return <span className="review-thread-undated">
      {event.kind === "finding" ? "Finding time unavailable in review report" : "Annotation time unavailable"}
    </span>;
  }
  const source = event.timeSource === "batch_staged_at"
    ? "Batch staged"
    : event.timeSource === "evidence_captured_at" ? "Captured" : "Annotated";
  return <time dateTime={event.occurredAt}>{source} · {event.occurredAt}</time>;
}

function ThreadEvent({ event }: { event: ReviewThreadEvent }) {
  let title: string;
  let detail: string;
  let secondary: string | null = null;
  switch (event.kind) {
    case "operation":
      title = `Operation ${event.operation.index + 1} · ${event.operation.op} ${event.operation.target}`;
      detail = event.operation.summary;
      secondary = event.operation.reason;
      break;
    case "finding":
      title = `${event.finding.severity} finding · ${event.finding.rule_id}`;
      detail = event.finding.message;
      secondary = event.finding.subject_name;
      break;
    case "evidence":
      title = `Evidence · ${event.evidence.title}`;
      detail = event.evidence.excerpt;
      secondary = `${event.evidence.source} · ${event.evidence.locator}`;
      break;
    case "annotation":
      title = `Annotation · ${event.annotation.actor}`;
      detail = event.annotation.body;
      break;
  }
  return (
    <article className="review-thread-event" data-thread-event-kind={event.kind} data-thread-event-key={event.key}>
      <div className="avatar" aria-hidden="true">{event.kind[0].toUpperCase()}</div>
      <div>
        <div className="activity-heading"><strong>{title}</strong>{eventTime(event)}</div>
        <p>{detail}</p>
        {secondary && <small className="review-thread-event-meta">{secondary}</small>}
        {event.kind === "operation" && (event.operation.before || event.operation.after) && (
          <details className="review-thread-record-values">
            <summary>Record values</summary>
            {event.operation.before && <div><strong>Before</strong><pre>{JSON.stringify(event.operation.before, null, 2)}</pre></div>}
            {event.operation.after && <div><strong>After</strong><pre>{JSON.stringify(event.operation.after, null, 2)}</pre></div>}
          </details>
        )}
      </div>
    </article>
  );
}

function EvidenceNotice({ unit, onImport }: { unit: ReviewThreadUnit; onImport?: () => void }) {
  const { evidence } = unit;
  if (evidence.status === "available") return null;
  if (evidence.status === "unlinked") {
    if (onImport) {
      return <DataState className="review-thread-evidence-state" state="empty" title="No linked evidence" message={evidence.reason} action={{ label: "Import another review bundle", onClick: onImport }} />;
    }
    return <section className="data-state empty review-thread-evidence-state" data-state="empty" role="status"><strong>No linked evidence</strong><p>{evidence.reason}</p></section>;
  }
  if (evidence.status === "truncated") {
    return (
      <DataState
        className="review-thread-evidence-state"
        state="truncated"
        title="Linked evidence is incomplete"
        shown={evidence.linkedIds.length - evidence.unresolvedIds.length}
        reason={evidence.reason}
        context={evidence.unresolvedIds}
      />
    );
  }
  return (
    <DataState
      className="review-thread-evidence-state"
      state="unavailable"
      title={evidence.status === "missing" ? "Linked evidence missing" : "Evidence unavailable"}
      message={evidence.reason}
      context={evidence.unresolvedIds}
    />
  );
}

/** The list and its selected thread use the same exact unit key for both report variants. */
export function ReviewThreadSurface({
  input,
  selectedUnitKey,
  onSelectUnit,
  annotations,
  query = "",
  onQuery,
  onImport,
  draft,
  onDraft,
  onAddLocalNote,
}: ReviewThreadSurfaceProps) {
  const model = useMemo(() => buildReviewThreadModel(input, annotations), [input, annotations]);
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const visibleUnits = model.units.filter((unit) => matchesQuery(unit, normalizedQuery));
  const selectedUnit = selectedUnitKey
    ? model.units.find((unit) => unit.key === selectedUnitKey)
    : model.units[0];
  const canAnnotate = onDraft !== undefined && onAddLocalNote !== undefined;

  function submitAnnotation(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (draft?.trim() && selectedUnit) onAddLocalNote?.();
  }

  return (
    <div className="view-stack review-thread-surface" data-review-thread-surface>
      <div className="surface-toolbar">
        <div><span className="eyebrow">Reviewable units</span><h2>{model.units.length} graph changes</h2></div>
        {onQuery && (
          <label className="filter-input review-thread-filter">
            <span>Filter</span>
            <input
              value={query}
              onChange={(event) => onQuery(event.target.value)}
              placeholder="Filter entities, edges, tiers…"
              aria-label="Filter reviewable units"
            />
            {query && <button type="button" onClick={() => onQuery("")} aria-label="Clear filter text">×</button>}
          </label>
        )}
      </div>
      <div className="review-thread-layout">
        <section className="review-thread-list-pane" aria-label="Reviewable units">
          <div className="review-thread-pane-heading">
            <strong>Changes</strong><span>{visibleUnits.length} of {model.units.length} units</span>
          </div>
          <div className="change-list review-thread-unit-list" data-keyboard-list>
            {visibleUnits.map((unit) => (
              <UnitRow
                key={unit.key}
                unit={unit}
                selected={selectedUnit?.key === unit.key}
                onSelect={() => onSelectUnit(unit.key)}
              />
            ))}
            {visibleUnits.length === 0 && <EmptyUnits query={query} onQuery={onQuery} onImport={onImport} />}
          </div>
          {input.review_kind === "pull_request" && (input.changes.truncated || input.changes.next_cursor) && (
            <DataState
              className="page-notice"
              state="truncated"
              title="Graph changes are incomplete"
              shown={input.changes.items.length}
              reason={input.changes.truncated
                ? "The export budget truncated the semantic-change page."
                : "Another semantic-change page exists but is not loaded in this review."}
              context={input.changes.next_cursor ? ["cursor available"] : undefined}
            />
          )}
          {input.review_kind === "pull_request" && input.enrichment_status.semantic_changes === "unavailable" && (
            <DataState
              state="unavailable"
              title="Semantic changes unavailable"
              message="This review bundle does not contain semantic-change enrichment; operation rows remain available."
            />
          )}
        </section>
        <section className="review-thread-pane" aria-label="Selected unit thread" data-review-thread>
          {selectedUnit ? (
            <>
              <div className="review-thread-pane-heading">
                <div><span className="eyebrow">{selectedUnit.substrate} thread</span><h3>{selectedUnit.title}</h3><UnitOntologyMark unit={selectedUnit} /></div>
                <span className={`tier-pill ${selectedUnit.tier}`}>Tier {selectedUnit.tier === "tier_2" ? "2" : "1"}</span>
              </div>
              <code className="record-id review-thread-selected-id">{selectedUnit.id}</code>
              <EvidenceNotice unit={selectedUnit} onImport={onImport} />
              {selectedUnit.events.length > 0 ? (
                <div className="activity-list review-thread-events" aria-label="Unit events">
                  {selectedUnit.events.map((event) => <ThreadEvent key={event.key} event={event} />)}
                </div>
              ) : (
                <section className="empty-state" data-state="empty" role="status">
                  <strong>No dated or undated events for this unit</strong>
                  <p>This semantic change has no operation, linked evidence, finding, or annotation in the loaded report.</p>
                </section>
              )}
              {canAnnotate && (
                <form className="review-thread-composer" onSubmit={submitAnnotation}>
                  <label htmlFor="review-thread-draft">Review comment</label>
                  <textarea
                    id="review-thread-draft"
                    value={draft ?? ""}
                    onChange={(event) => onDraft?.(event.target.value)}
                    placeholder="Write a note for this review session…"
                  />
                  <button className="button primary" type="submit" disabled={!draft?.trim()}>Add local note</button>
                </form>
              )}
            </>
          ) : (
            <DataState
              state="unavailable"
              title={selectedUnitKey ? "Selected unit unavailable" : "No unit selected"}
              message={selectedUnitKey
                ? "The selected entity, note, or edge key does not appear in this review report."
                : "The loaded review report has no unit to open."}
              context={selectedUnitKey ? [selectedUnitKey] : undefined}
            />
          )}
        </section>
      </div>
      {(model.unlinkedFindings.length > 0 || model.unlinkedAnnotations.length > 0 ||
        model.unassignedEvidence.length > 0 || model.activityStatus !== "empty") && (
        <aside className="review-thread-unlinked" aria-label="Review material without a unit link">
          {model.unlinkedFindings.length > 0 && (
            <p>{model.unlinkedFindings.length} finding{model.unlinkedFindings.length === 1 ? "" : "s"} cannot enter a unit thread because the subject ID is absent or ambiguous across substrates.</p>
          )}
          {model.unlinkedAnnotations.length > 0 && (
            <p>{model.unlinkedAnnotations.length} local annotation{model.unlinkedAnnotations.length === 1 ? "" : "s"} name a unit that is not in this report.</p>
          )}
          {model.unassignedEvidence.length > 0 && (
            <p>{model.unassignedEvidence.length} evidence item{model.unassignedEvidence.length === 1 ? "" : "s"} cannot enter a unit thread. {model.unassignedEvidenceReason}</p>
          )}
          {model.activityStatus !== "empty" && <p>{model.activityReason}</p>}
        </aside>
      )}
    </div>
  );
}
