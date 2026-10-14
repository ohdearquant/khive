//! Statement labels carried from a write's submit site to its writer-stage
//! observation and `slow_write` row: the label bound, first-seen
//! deduplication, and the labelled submit entry points on
//! [`WriterTaskHandle`].

use khive_storage::error::{StorageError, WriterTaskRequestState};
use khive_storage::types::SqlStatement;
use rusqlite::Connection;

use super::{writer_task_terminated, WriterTaskHandle};

/// Upper bound on distinct statement labels carried on one request. A
/// request whose statements name more than this contributes its first
/// [`MAX_WRITE_LABELS`] distinct labels to the writer-stage observation and
/// `slow_write` row; the dropped distinct count is reported separately by
/// the sink.
pub(crate) const MAX_WRITE_LABELS: usize = 8;

/// Append `label` to `labels` if it is not already present, in first-seen
/// order.
fn push_unique_label(labels: &mut Vec<String>, label: &str) {
    if !labels.iter().any(|existing| existing == label) {
        labels.push(label.to_owned());
    }
}

/// Collect the distinct statement labels a statement list carries, in
/// first-seen order and deduplicated but unbounded — the bound is applied by
/// the writer's slow-path label resolution so a dropped distinct count can be
/// reported. Used by submit sites that hold their [`SqlStatement`]s (the
/// `sql_bridge` execute paths); an opaque caller submits
/// [`SubmitLabels::None`] or [`SubmitLabels::Site`] instead.
pub(crate) fn collect_statement_labels(statements: &[SqlStatement]) -> Vec<String> {
    dedup_labels(
        statements
            .iter()
            .filter_map(|statement| statement.label.as_deref()),
    )
}

/// Deduplicate labels in first-seen order.
pub(super) fn dedup_labels<'a>(labels: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut deduped = Vec::new();
    for label in labels {
        push_unique_label(&mut deduped, label);
    }
    deduped
}

/// What the submit site knows about the statements this request will run.
///
/// The write queue accepts opaque closures, so a request's statements are not
/// generally inspectable at submit time — only the few sites that hold a
/// built [`khive_storage::types::SqlStatement`] list do. This carries that
/// knowledge to the writer-stage observation without changing the fast path
/// for the common case.
#[derive(Clone, Debug)]
pub(super) enum SubmitLabels {
    /// The submit site has nothing to name (an opaque closure from a caller
    /// with no operation in hand). Contributes an empty label list.
    None,
    /// The submit site's own `&'static str` operation name. Materialized into
    /// an owned string only when the write is slow.
    Site(&'static str),
    /// Label strings the caller already collected from its statements, in
    /// first-seen order and deduplicated. `sql_bridge`'s `execute` /
    /// `execute_batch` build these from the statements they hold.
    Statements(Vec<String>),
}

impl WriterTaskHandle {
    /// Like [`Self::send`], but carries `labels` (the submit site's
    /// [`SqlStatement::label`]s, in first-seen order) onto the request's
    /// writer-stage observation and `slow_write` row. Production submit
    /// sites use [`Self::send_bounded_labelled`]; this unbounded form exists
    /// for tests.
    #[cfg(test)]
    pub(crate) async fn send_labelled<R, F>(
        &self,
        op: F,
        labels: Vec<String>,
    ) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let reply_rx = self
            .enqueue_inner(op, false, false, false, SubmitLabels::Statements(labels))
            .await?;
        reply_rx
            .await
            .map_err(|_| writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown))?
    }

    /// Like [`Self::send`], but carries the submit site's operation name
    /// `site` onto the request's writer-stage observation and `slow_write`
    /// row. Used by store call sites that hold a `&'static str` operation
    /// (the same name they already pass for error mapping) but no statement
    /// list.
    pub(crate) async fn send_at<R, F>(&self, op: F, site: &'static str) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        let reply_rx = self
            .enqueue_inner(op, false, false, false, SubmitLabels::Site(site))
            .await?;
        reply_rx
            .await
            .map_err(|_| writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown))?
    }

    /// Like [`Self::send_bounded`], but carries `labels` (the submit site's
    /// [`SqlStatement::label`]s) onto the request's writer-stage observation
    /// and `slow_write` row. See [`Self::send_labelled`].
    pub(crate) async fn send_bounded_labelled<R, F>(
        &self,
        op: F,
        labels: Vec<String>,
    ) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        self.send_with_timeout_labelled(op, self.enqueue_timeout, SubmitLabels::Statements(labels))
            .await
    }

    /// Like [`Self::send_bounded`], but carries the submit site's operation
    /// name `site` onto the request's writer-stage observation and
    /// `slow_write` row. See [`Self::send_at`].
    pub(crate) async fn send_bounded_at<R, F>(
        &self,
        op: F,
        site: &'static str,
    ) -> Result<R, StorageError>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R, StorageError> + Send + 'static,
    {
        self.send_with_timeout_labelled(op, self.enqueue_timeout, SubmitLabels::Site(site))
            .await
    }
}
