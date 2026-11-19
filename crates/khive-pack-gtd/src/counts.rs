//! `gtd.counts`: per-assignee task counts without task bodies.
//!
//! A read verb for callers that only need numbers (a prompt header) or the open
//! rows of one session tag (a stop hook). It never loads task rows to count
//! them: every number comes from an aggregate query, and the optional tag block
//! lists at most `limit` rows carrying five fields each.
//!
//! Filter construction mirrors `gtd.tasks` (`handle_tasks` in `handlers.rs`) so
//! the two reads agree for the same caller: same namespace scope, same
//! `FilterOp::TextEqOrNonText` / `TextInOrNonText` legacy-status fallback, same
//! exact `$.assignee` match, same tag matching, and the same `Active` note-status
//! selector, which is what keeps soft-deleted tasks out of every number.

use serde::Deserialize;
use serde_json::{json, Map, Value};

use khive_runtime::deser_params as deser;
use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::note::{
    FilterOp, Note, NoteFilter, NoteStatusSelector, NoteTimeOrder, PropertyFilter,
};
use khive_storage::SqlValue;

use crate::handlers::{render_task, DEFAULT_PRIORITY, DEFAULT_STATUS};
use crate::schema::{is_terminal, TASK_STATUSES};
use crate::GtdPack;

/// Rows returned in the tag block when `limit` is not given.
const DEFAULT_ROW_LIMIT: u32 = 50;
/// Largest accepted `limit`.
const MAX_ROW_LIMIT: u32 = 200;
/// Canonical priorities, in the order rows are listed.
const CANONICAL_PRIORITIES: [&str; 4] = ["p0", "p1", "p2", "p3"];
/// One bucket per canonical priority plus one for every other text priority.
const PRIORITY_BUCKETS: usize = CANONICAL_PRIORITIES.len() + 1;

/// `gtd.counts` parameters. Unknown keys are refused, like every gtd params struct.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CountsParams {
    /// Actor whose tasks are counted; matched exactly against `properties.assignee`.
    assignee: String,
    /// Exact tag whose open tasks are listed (`session:oss-reviews`).
    tag: Option<String>,
    /// Row cap for the tag block, 1..=200, default 50. Validated even when `tag` is absent.
    limit: Option<u32>,
}

/// Namespaces the caller can see, in the shape `NoteFilter.namespaces` expects.
/// Empty (single-namespace caller) defers to the namespace argument, exactly as
/// `handle_tasks` does.
fn visible_namespaces(token: &NamespaceToken) -> Vec<String> {
    if token.visible_namespaces().len() > 1 {
        token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect()
    } else {
        Vec::new()
    }
}

/// Exact `$.assignee` match, as in `handle_tasks`.
fn assignee_filter(assignee: &str) -> PropertyFilter {
    PropertyFilter {
        json_path: "$.assignee".to_string(),
        op: FilterOp::Eq,
        value: SqlValue::Text(assignee.to_string()),
    }
}

/// Filter selecting one status. A legacy task whose stored status is absent or
/// non-text reads as `inbox` everywhere in this pack, so `inbox` uses the
/// storage predicate that reproduces that read model.
fn status_filter(status: &str) -> PropertyFilter {
    PropertyFilter {
        json_path: "$.status".to_string(),
        op: if status == DEFAULT_STATUS {
            FilterOp::TextEqOrNonText
        } else {
            FilterOp::Eq
        },
        value: SqlValue::Text(status.to_string()),
    }
}

/// Filter selecting every open status (non-terminal), legacy status as `inbox`.
fn open_status_filter() -> PropertyFilter {
    let open_statuses: Vec<SqlValue> = TASK_STATUSES
        .iter()
        .filter(|status| !is_terminal(status))
        .map(|status| SqlValue::Text((*status).to_string()))
        .collect();
    PropertyFilter {
        json_path: "$.status".to_string(),
        op: FilterOp::TextInOrNonText(open_statuses),
        value: SqlValue::Null,
    }
}

/// Filters selecting priority bucket `bucket` (0..PRIORITY_BUCKETS). Buckets are
/// disjoint and cover every task: p0, p1, p2 (including a missing or non-text
/// priority, which `render_task` renders as `p2`), p3, then any other text.
fn priority_bucket_filters(bucket: usize) -> Vec<PropertyFilter> {
    let json_path = "$.priority".to_string();
    match CANONICAL_PRIORITIES.get(bucket) {
        Some(&want) => vec![PropertyFilter {
            json_path,
            op: if want == DEFAULT_PRIORITY {
                FilterOp::TextEqOrNonText
            } else {
                FilterOp::Eq
            },
            value: SqlValue::Text(want.to_string()),
        }],
        None => vec![
            PropertyFilter {
                json_path: json_path.clone(),
                op: FilterOp::NotInOrMissing(
                    CANONICAL_PRIORITIES
                        .iter()
                        .map(|p| SqlValue::Text((*p).to_string()))
                        .collect(),
                ),
                value: SqlValue::Null,
            },
            PropertyFilter {
                json_path,
                op: FilterOp::JsonTypeEq,
                value: SqlValue::Text("text".to_string()),
            },
        ],
    }
}

/// A task-kind filter in the caller's namespace scope. `Active` note status
/// excludes soft-deleted tasks, as `gtd.tasks` does.
fn task_filter(
    namespaces: &[String],
    property_filters: Vec<PropertyFilter>,
    tags: Vec<String>,
) -> NoteFilter {
    NoteFilter {
        kind: Some("task".to_string()),
        property_filters,
        namespaces: namespaces.to_vec(),
        tags,
        ..Default::default()
    }
    .with_default_note_status(NoteStatusSelector::Active)
}

/// Project a task to the five fields a counts row carries, using the same
/// renderer as `gtd.tasks` so the forms agree. No description, properties or
/// any other field is copied.
fn project_row(note: &Note) -> Value {
    let rendered = render_task(note);
    json!({
        "id": rendered["id"].clone(),
        "title": rendered["title"].clone(),
        "status": rendered["status"].clone(),
        "priority": rendered["priority"].clone(),
        "updated_at": rendered["updated_at"].clone(),
    })
}

impl GtdPack {
    /// `gtd.counts(assignee, tag?, limit?)`.
    ///
    /// Always returns `{assignee, by_status, open, total}` with one `by_status`
    /// entry for every status in `TASK_STATUSES` (zeros included). `open` sums
    /// the non-terminal statuses and `total` sums all of them. A stored status
    /// outside `TASK_STATUSES` is counted nowhere, matching `gtd.tasks`, which
    /// lists such a task under no status.
    ///
    /// With `tag`, also returns `tag: {name, open, rows, truncated}`: the
    /// assignee's open tasks carrying exactly that tag, at most `limit` rows,
    /// ordered p0, p1, p2, p3, other, then `updated_at` oldest first.
    /// `truncated` is true exactly when `open` exceeds the rows returned.
    pub(crate) async fn handle_counts(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: CountsParams = deser(params)?;
        if let Some(tag) = p.tag.as_deref() {
            if tag.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "tag must not be empty".to_string(),
                ));
            }
        }
        // Refused, not clamped: the result has no field to report a clamp in.
        let limit = p.limit.unwrap_or(DEFAULT_ROW_LIMIT);
        if !(1..=MAX_ROW_LIMIT).contains(&limit) {
            return Err(RuntimeError::InvalidInput(format!(
                "limit must be 1..={MAX_ROW_LIMIT}"
            )));
        }

        let namespaces = visible_namespaces(token);
        let mut filters: Vec<NoteFilter> = TASK_STATUSES
            .iter()
            .map(|status| {
                task_filter(
                    &namespaces,
                    vec![status_filter(status), assignee_filter(&p.assignee)],
                    Vec::new(),
                )
            })
            .collect();
        if let Some(tag) = p.tag.as_ref() {
            filters.push(task_filter(
                &namespaces,
                vec![open_status_filter(), assignee_filter(&p.assignee)],
                vec![tag.clone()],
            ));
        }

        let notes = self.runtime().notes(token)?;
        let counts = notes
            .count_notes_filtered_in_snapshot(token.namespace().as_str(), &filters)
            .await
            .map_err(|e| {
                RuntimeError::Internal(format!("count_notes_filtered_in_snapshot: {e}"))
            })?;
        if counts.len() != filters.len() {
            return Err(RuntimeError::Internal(format!(
                "count_notes_filtered_in_snapshot returned {} counts for {} filters",
                counts.len(),
                filters.len()
            )));
        }

        let mut by_status = Map::new();
        let mut open: u64 = 0;
        let mut total: u64 = 0;
        for (status, count) in TASK_STATUSES.iter().zip(counts.iter()) {
            let count = *count;
            by_status.insert((*status).to_string(), json!(count));
            total += count;
            if !is_terminal(status) {
                open += count;
            }
        }
        let mut result = json!({
            "assignee": p.assignee,
            "by_status": by_status,
            "open": open,
            "total": total,
        });

        if let Some(tag) = p.tag.as_ref() {
            let tag_open = counts[TASK_STATUSES.len()];
            let mut rows: Vec<Value> = Vec::new();
            for bucket in 0..PRIORITY_BUCKETS {
                let remaining = limit as usize - rows.len();
                if remaining == 0 || (rows.len() as u64) >= tag_open {
                    break;
                }
                let mut property_filters = vec![open_status_filter(), assignee_filter(&p.assignee)];
                property_filters.extend(priority_bucket_filters(bucket));
                let mut filter = task_filter(&namespaces, property_filters, vec![tag.clone()]);
                filter.time_order = Some(NoteTimeOrder::UpdatedAt);
                // The bounded read returns up to `max_rows + 1` rows; only the
                // remaining slots are kept.
                let page = notes
                    .query_notes_filtered_bounded(
                        token.namespace().as_str(),
                        &filter,
                        remaining as u32,
                    )
                    .await
                    .map_err(|e| {
                        RuntimeError::Internal(format!("query_notes_filtered_bounded: {e}"))
                    })?;
                rows.extend(page.iter().take(remaining).map(project_row));
            }
            let truncated = tag_open > rows.len() as u64;
            result["tag"] = json!({
                "name": tag,
                "open": tag_open,
                "rows": rows,
                "truncated": truncated,
            });
        }
        Ok(result)
    }
}
