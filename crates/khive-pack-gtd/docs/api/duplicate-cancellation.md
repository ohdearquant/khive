# Record a duplicate cancellation

Cancel a task and retain the task it duplicates with either lifecycle verb:

```text
gtd.transition(id="<duplicate task>", status="cancelled", duplicate_of="<kept task>")
gtd.complete(id="<duplicate task>", status="cancelled", duplicate_of="<kept task>")
```

`duplicate_of` accepts a full UUID or a unique hex prefix of at least eight
characters, using the same namespace-agnostic resolution as other lifecycle ids.
The kept record must be a live task; its status may be open, done, or cancelled.
Self references, non-task records, unknown/deleted targets and a destination
other than `cancelled` are refused without changing the task. There is no cycle
check. The pair records the caller's duplicate judgment, not a scheduling
relationship.

The cancellation and canonical UUID in `properties.duplicate_of` share one
conditional update. The update guards the source task's revision/version and
rechecks the partner's live task kind. Atomic CLI execution uses the same
statement, so an earlier operation that deletes the partner rolls back the
entire unit. Existing lifecycle audit writes remain best effort and report their
existing `audit_persisted` result. If that update matches no row, nothing is
written and the error names both possible causes: the task changed after the
decision, or the kept task is no longer a live task.

`properties.duplicate_of` is owned by the lifecycle verbs. A generic `update`
that names it, whether to set, change or clear it, is refused, and so is
creating a task whose properties carry it, so a recorded pair always comes from
a validated cancellation.

`get(id="<cancelled task>")` and `gtd.tasks` return the stored property. Find the
reverse references with:

```text
gtd.tasks(duplicate_of="<kept task>")
```

This filter defaults to `status="cancelled"` when status is omitted. An explicit
status still applies, together with ordinary namespace visibility, assignee,
priority, tags, context, limit and offset filters. Without `duplicate_of`, the
usual default continues to exclude terminal tasks. A full UUID filter can read
retained judgments after the kept task is deleted; prefix resolution still
requires an unambiguous live record.

The reverse task query is the accepted alternative to incoming graph links for
issue #4134. This feature creates no graph relation.

A same-status `gtd.transition(status="cancelled")` with `duplicate_of` succeeds
only when it exactly matches the recorded canonical UUID and the partner is
still live. It asserts the unchanged snapshot without updating the note or
appending an audit. A new or different judgment is refused. `gtd.complete` keeps
its existing terminal-source refusal. Cancels without `duplicate_of` retain
their existing response shape and behavior.
