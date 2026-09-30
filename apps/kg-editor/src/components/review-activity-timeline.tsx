"use client";

import { useMemo, useState } from "react";

import { DataState } from "@/components/data-state";
import {
  REVIEW_ACTIVITY_FACETS,
  reviewActivityEvents,
  reviewActivityFacet,
  type ReviewActivityFacet,
} from "@/lib/review-activity";
import type { ReviewInput } from "@/lib/review-bundle";

const facetLabels: Record<ReviewActivityFacet, string> = {
  all: "All activity",
  operations: "Operations",
  validation: "Validation",
  evidence: "Evidence",
  review: "Review events",
  conversation: "Conversation",
};

type Page = Readonly<{ items: readonly unknown[]; next_cursor: string | null; truncated: boolean }>;

function PageLimit({ page, label }: { page: Page; label: string }) {
  if (!page.truncated && page.next_cursor === null) return null;
  return (
    <DataState
      className="page-notice"
      state="truncated"
      title={`${label} are truncated`}
      shown={page.items.length}
      reason={page.truncated
        ? "The export budget truncated this source of timeline events."
        : "Another page exists, but this static review only shows the captured page."}
      context={page.next_cursor ? ["cursor available"] : undefined}
    />
  );
}

function unavailableReason(input: ReviewInput, facet: ReviewActivityFacet): string | null {
  if (input.review_kind === "changeset") {
    if (facet === "evidence" || facet === "conversation") {
      return "The shared CLI review report does not contain an evidence or conversation page.";
    }
    return null;
  }
  if (facet === "evidence" && input.enrichment_status.evidence === "unavailable") {
    return "This review bundle did not capture evidence enrichment.";
  }
  if (facet === "conversation" && input.enrichment_status.activity === "unavailable") {
    return "This review bundle did not capture conversation enrichment.";
  }
  return null;
}

function incompleteFacet(input: ReviewInput, facet: ReviewActivityFacet): boolean {
  if (input.review_kind !== "pull_request") return false;
  const page = facet === "validation" ? input.checks
    : facet === "evidence" ? input.evidence
      : facet === "conversation" ? input.activity : null;
  return page !== null && (page.truncated || page.next_cursor !== null);
}

export function ReviewActivityTimeline({
  input,
  selectedEventId,
  onSelectEvent,
}: {
  input: ReviewInput;
  selectedEventId: string;
  onSelectEvent: (id: string) => void;
}) {
  const [facet, setFacet] = useState<ReviewActivityFacet>("all");
  const events = useMemo(() => reviewActivityEvents(input), [input]);
  const visible = reviewActivityFacet(events, facet);
  const unavailable = unavailableReason(input, facet);
  const incomplete = incompleteFacet(input, facet);

  return (
    <div className="view-stack" data-review-activity-timeline>
      <div className="surface-toolbar">
        <div>
          <span className="eyebrow">Review activity</span>
          <h2>History</h2>
        </div>
        <span>{visible.length} of {events.length} events</span>
      </div>
      <div className="segmented-control" role="group" aria-label="Filter history by type">
        {REVIEW_ACTIVITY_FACETS.map((item) => (
          <button
            key={item}
            type="button"
            aria-pressed={facet === item}
            className={facet === item ? "active" : ""}
            onClick={() => setFacet(item)}
          >
            {facetLabels[item]}
          </button>
        ))}
      </div>
      {unavailable ? (
        <DataState state="unavailable" title={`${facetLabels[facet]} unavailable`} message={unavailable} />
      ) : visible.length === 0 && incomplete ? null : visible.length === 0 ? (
        <DataState
          state="empty"
          title={`No ${facetLabels[facet].toLocaleLowerCase()} captured`}
          message="No event of this type is present in the captured review."
          action={{ label: "Show all activity", onClick: () => setFacet("all") }}
        />
      ) : (
        <div className="activity-list" data-review-activity-rows data-keyboard-list>
          {visible.map((event) => (
            <article key={event.id} data-activity-kind={event.kind} className={selectedEventId === event.id ? "selected" : ""}>
              <div className="avatar" aria-hidden="true">{event.kind.slice(0, 1).toUpperCase()}</div>
              <button
                type="button"
                className="review-activity-event"
                data-keyboard-row
                aria-keyshortcuts="J K Enter"
                aria-pressed={selectedEventId === event.id}
                onClick={() => onSelectEvent(event.id)}
              >
                <div className="activity-heading">
                  <strong>{event.title}</strong>
                  <span>{facetLabels[event.kind]}</span>
                  <time dateTime={event.occurredAt === null ? undefined : new Date(event.occurredAt).toISOString()}>
                    {event.occurredAt === null
                      ? "Time not supplied by bundle"
                      : new Intl.DateTimeFormat("en", { dateStyle: "medium", timeStyle: "short" }).format(event.occurredAt)}
                  </time>
                </div>
                <p>{event.detail}</p>
              </button>
            </article>
          ))}
        </div>
      )}
      {input.review_kind === "pull_request" && (facet === "all" || facet === "validation") && (
        <PageLimit page={input.checks} label="Checks" />
      )}
      {input.review_kind === "pull_request" && (facet === "all" || facet === "evidence") && (
        <PageLimit page={input.evidence} label="Evidence anchors" />
      )}
      {input.review_kind === "pull_request" && (facet === "all" || facet === "conversation") && (
        <PageLimit page={input.activity} label="Conversation events" />
      )}
      {input.review_kind === "pull_request" && facet === "all" &&
        input.enrichment_status.evidence === "unavailable" && (
          <DataState
            state="unavailable"
            title="Evidence unavailable"
            message="This review bundle did not capture evidence enrichment."
          />
        )}
      {input.review_kind === "pull_request" && facet === "all" &&
        input.enrichment_status.activity === "unavailable" && (
          <DataState
            state="unavailable"
            title="Conversation unavailable"
            message="This review bundle did not capture conversation enrichment."
          />
        )}
    </div>
  );
}
