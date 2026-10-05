//! Pure SQL construction for ANN corpus reads, counts and log probes.
//!
//! Readers, parsing, classification thresholds and error mapping stay with the caller.

use khive_storage::types::{SqlStatement, SqlValue};

use super::registry::CompactionScope;

/// Live-row join applied to corpus reads and counts, never to the write-log tail.
#[derive(Clone, Copy, Debug)]
pub enum LiveRowJoin {
    /// Join vector subject IDs to non-deleted notes.
    Notes,
}

/// Watermark captured inside the corpus scan's read snapshot.
#[derive(Clone, Copy, Debug)]
pub enum WatermarkCapture<'a> {
    /// Retained scope maximum, floored by this consumer's nonnegative watermark.
    ScopedMaximumWithOwnFloor {
        /// Consumer whose prior active checkpoint protects the captured prefix.
        consumer: &'a str,
        /// Registry namespace for that consumer, including the global wildcard.
        registry_namespace: &'a str,
    },
    /// The write log's AUTOINCREMENT high-water, which survives compaction.
    LogHighWater,
}

/// Corpus predicates and capture rule shared by an ANN consumer's statements.
#[derive(Clone, Copy, Debug)]
pub struct CorpusScope<'a> {
    /// Restrict to one namespace, or span every namespace when absent.
    pub namespace: Option<&'a str>,
    /// Optional vector/log record kind, emitted as an escaped SQL literal.
    pub record_kind: Option<&'static str>,
    /// Vector/log field, emitted as an escaped SQL literal.
    pub field: &'static str,
    /// Optional live-row join on the vector corpus.
    pub live_join: Option<LiveRowJoin>,
    /// Capture rule chosen by the pack; never substitutes another scope's rule.
    pub watermark_capture: WatermarkCapture<'a>,
}

impl CorpusScope<'_> {
    fn predicate(&self, alias: &str) -> String {
        let mut terms = Vec::new();
        let model_param = if self.namespace.is_some() {
            terms.push(format!("{alias}namespace = ?1"));
            2
        } else {
            1
        };
        terms.push(format!("{alias}embedding_model = ?{model_param}"));
        if let Some(kind) = self.record_kind {
            terms.push(format!("{alias}kind = '{}'", kind.replace('\'', "''")));
        }
        terms.push(format!(
            "{alias}field = '{}'",
            self.field.replace('\'', "''")
        ));
        terms.join(" AND ")
    }

    fn params(&self, model: &str, watermark: u64) -> Vec<SqlValue> {
        let mut params = Vec::new();
        if let Some(namespace) = self.namespace {
            params.push(SqlValue::Text(namespace.to_owned()));
        }
        params.push(SqlValue::Text(model.to_owned()));
        // Preserve the callers' existing watermark representation.
        params.push(SqlValue::Integer(watermark as i64));
        params
    }

    fn corpus(&self, table_name: &str) -> (String, String) {
        match self.live_join {
            Some(LiveRowJoin::Notes) => (
                format!("{table_name} v JOIN notes n ON n.id = v.subject_id"),
                format!("{} AND n.deleted_at IS NULL", self.predicate("v.")),
            ),
            None => (table_name.to_owned(), self.predicate("")),
        }
    }

    fn model_params(&self, model: &str) -> Vec<SqlValue> {
        let mut params = Vec::new();
        if let Some(namespace) = self.namespace {
            params.push(SqlValue::Text(namespace.to_owned()));
        }
        params.push(SqlValue::Text(model.to_owned()));
        params
    }

    fn capture_expression(&self, params: &mut Vec<SqlValue>) -> String {
        match self.watermark_capture {
            WatermarkCapture::ScopedMaximumWithOwnFloor {
                consumer,
                registry_namespace,
            } => {
                let model_param = params.len();
                params.push(SqlValue::Text(consumer.to_owned()));
                let consumer_param = params.len();
                params.push(SqlValue::Text(registry_namespace.to_owned()));
                let namespace_param = params.len();
                let predicate = self.predicate("");
                format!(
                    "MAX( \
                       (SELECT COALESCE(MAX(seq), 0) FROM ann_write_log \
                         WHERE {predicate}), \
                       (SELECT COALESCE(MAX(watermark), 0) \
                          FROM ann_consumer_watermark \
                         WHERE consumer = ?{consumer_param} AND namespace = ?{namespace_param} \
                           AND embedding_model = ?{model_param} AND watermark >= 0) \
                     )"
                )
            }
            WatermarkCapture::LogHighWater => "(SELECT COALESCE(\
                (SELECT seq FROM sqlite_sequence \
                 WHERE name = 'ann_write_log'), 0))"
                .into(),
        }
    }

    /// Count the live corpus for a fingerprint; dimensions remain caller-owned.
    ///
    /// `table_name` must be the caller's trusted, sanitized vector table identifier.
    pub fn fingerprint(&self, table_name: &str, model: &str, label: &str) -> SqlStatement {
        let (corpus, live) = self.corpus(table_name);
        SqlStatement {
            sql: format!("SELECT COUNT(*) AS n FROM {corpus} WHERE {live}"),
            params: self.model_params(model),
            label: Some(label.to_owned()),
        }
    }

    /// Read ordered vectors and capture their watermark in one statement.
    ///
    /// A note join also returns each live note's namespace for the global index.
    /// `table_name` must be the caller's trusted, sanitized vector table identifier.
    pub fn corpus_scan(&self, table_name: &str, model: &str, label: &str) -> SqlStatement {
        let mut params = self.model_params(model);
        let capture = self.capture_expression(&mut params);
        let (corpus, live) = self.corpus(table_name);
        let (columns, order) = match self.live_join {
            Some(LiveRowJoin::Notes) => ("v.subject_id, v.embedding, n.namespace", "v.subject_id"),
            None => ("subject_id, embedding", "subject_id"),
        };
        SqlStatement {
            sql: format!(
                "SELECT {columns}, {capture} AS log_s FROM {corpus} WHERE {live} ORDER BY {order}"
            ),
            params,
            label: Some(label.to_owned()),
        }
    }

    /// Select the registry compaction boundary without changing its lifecycle.
    pub fn compaction_scope(&self) -> CompactionScope {
        match self.namespace {
            Some(namespace) => CompactionScope::Namespace(namespace.to_owned()),
            None => CompactionScope::Model,
        }
    }

    /// Probe only the log for a row beyond the captured watermark.
    pub fn tail_exists(&self, model: &str, watermark: u64, label: &str) -> SqlStatement {
        let params = self.params(model, watermark);
        let seq_param = params.len();
        let predicate = self.predicate("");
        SqlStatement {
            sql: format!("SELECT EXISTS(SELECT 1 FROM ann_write_log WHERE {predicate} AND seq > ?{seq_param}) AS has_tail"),
            params,
            label: Some(label.to_owned()),
        }
    }

    /// Count corpus and tail in one statement/read snapshot.
    ///
    /// `table_name` must be the caller's trusted, sanitized vector table identifier.
    pub fn scope_counts(
        &self,
        table_name: &str,
        model: &str,
        watermark: u64,
        label: &str,
    ) -> SqlStatement {
        let params = self.params(model, watermark);
        let seq_param = params.len();
        let (corpus, live) = self.corpus(table_name);
        let tail = self.predicate("");
        SqlStatement {
            sql: format!("SELECT (SELECT COUNT(*) FROM {corpus} WHERE {live}) AS live, (SELECT COUNT(*) FROM ann_write_log WHERE {tail} AND seq > ?{seq_param}) AS tail"),
            params,
            label: Some(label.to_owned()),
        }
    }

    /// Bound the classification corpus count with the caller's existing multiplier.
    ///
    /// `table_name` must be the caller's trusted, sanitized vector table identifier.
    /// A NULL multiplier preserves the caller's unbounded-count fallback.
    pub fn classification_scope_counts(
        &self,
        table_name: &str,
        model: &str,
        watermark: u64,
        multiplier: SqlValue,
        label: &str,
    ) -> SqlStatement {
        let mut params = self.params(model, watermark);
        let seq_param = params.len();
        params.push(multiplier);
        let cap_param = params.len();
        let (corpus, live) = self.corpus(table_name);
        let tail = self.predicate("");
        SqlStatement {
            sql: format!(
                "WITH tail AS MATERIALIZED (\
                   SELECT COUNT(*) AS tail_rows FROM ann_write_log \
                   WHERE {tail} AND seq > ?{seq_param}\
                 ), cap AS MATERIALIZED (\
                   SELECT CASE \
                     WHEN tail_rows = 1 THEN 1 \
                     WHEN tail_rows = 0 OR ?{cap_param} IS NULL \
                       OR tail_rows > 9223372036854775807 / ?{cap_param} THEN -1 \
                     ELSE tail_rows * ?{cap_param} END AS max_rows FROM tail\
                 ), live AS (\
                   SELECT COUNT(*) AS live_rows FROM (\
                     SELECT 1 FROM {corpus} \
                     WHERE {live} \
                     LIMIT (SELECT max_rows FROM cap)\
                   )\
                 ) \
                 SELECT live.live_rows AS live, tail.tail_rows AS tail, \
                        cap.max_rows AS cap FROM live CROSS JOIN tail CROSS JOIN cap"
            ),
            params,
            label: Some(label.to_owned()),
        }
    }
}
