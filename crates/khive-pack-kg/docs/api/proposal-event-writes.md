# Proposal event writes

Proposal projection transitions and their events commit in one batch. The
projection compare-and-set runs first; an event insert runs only when that
statement changed one row. A missed transition writes neither an event nor
derived observation rows.

The projection worker obtains event and observation bindings from the runtime's
`event_insert_statements` re-export. Conditional SQL stays in the pack's
declarative SQL assets. Each inserted row affects one row, so `changes() = 1`
chains the transition, event and all observations in order. Any insertion error
rolls back the entire batch.

The shared builder validates paired operation attribution, the profile version's
signed storage range, and observation payloads before acquiring the writer or
mutating the projection. Invalid event metadata fails explicitly. Event usage
counts one event regardless of how many observation rows it derives; a missed
transition counts zero.

Ordinary proposal events derive no observation rows and retain their existing
insert columns, bindings, labels and transition semantics. Events with derived
observations now persist those rows alongside the event instead of discarding
them. Public projection-worker methods retain their signatures. This adopts the
shared event binding builder for one part of #4532; it does not decide the unused
public callback API or change unrelated pagination and proposal handlers.
