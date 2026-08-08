//! Offline retrieval-quality evaluation for the knowledge corpus.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::Utc;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::util::sql_err;
use super::{vamana, KnowledgeHandlers};

const EVAL_K: usize = 5;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalRetrievalParams {
    query_set: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuerySetFile {
    #[serde(default)]
    queries: Vec<QuerySetEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuerySetEntry {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    expected_slugs: Option<Vec<String>>,
}

#[derive(Debug)]
struct ValidatedQuery {
    query: String,
    expected_slugs: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct QueryMetrics {
    precision_at_5: f64,
    recall_at_5: f64,
    mrr: f64,
}

fn parse_query_set(path: &Path, contents: &str) -> Result<Vec<ValidatedQuery>, RuntimeError> {
    let parsed: QuerySetFile = toml::from_str(contents).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "knowledge.eval_retrieval: invalid TOML syntax in query set {}: {error}",
            path.display()
        ))
    })?;
    if parsed.queries.is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "knowledge.eval_retrieval: query set {} contains no queries",
            path.display()
        )));
    }

    parsed
        .queries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let query = entry.query.unwrap_or_default().trim().to_string();
            if query.is_empty() {
                return Err(RuntimeError::InvalidInput(format!(
                    "knowledge.eval_retrieval: query set {} entry [{index}] has empty query text",
                    path.display()
                )));
            }
            let expected_slugs = entry.expected_slugs.unwrap_or_default();
            if expected_slugs.is_empty() {
                return Err(RuntimeError::InvalidInput(format!(
                    "knowledge.eval_retrieval: query set {} entry [{index}] has empty expected_slugs",
                    path.display()
                )));
            }
            let expected_slugs: HashSet<String> = expected_slugs
                .into_iter()
                .map(|slug| slug.trim().to_string())
                .collect();
            if expected_slugs.iter().any(String::is_empty) {
                return Err(RuntimeError::InvalidInput(format!(
                    "knowledge.eval_retrieval: query set {} entry [{index}] contains an empty expected slug",
                    path.display()
                )));
            }
            Ok(ValidatedQuery {
                query,
                expected_slugs,
            })
        })
        .collect()
}

fn score_query(expected_slugs: &HashSet<String>, returned_slugs: &[String]) -> QueryMetrics {
    let mut top_five = returned_slugs.iter().take(EVAL_K);
    let returned_set: HashSet<&str> = top_five.clone().map(String::as_str).collect();
    let hits = expected_slugs
        .iter()
        .filter(|slug| returned_set.contains(slug.as_str()))
        .count();
    let mrr = top_five
        .position(|slug| expected_slugs.contains(slug))
        .map(|position| 1.0 / (position + 1) as f64)
        .unwrap_or(0.0);
    QueryMetrics {
        precision_at_5: hits as f64 / EVAL_K as f64,
        recall_at_5: hits as f64 / expected_slugs.len() as f64,
        mrr,
    }
}

impl KnowledgeHandlers {
    pub(crate) async fn eval_retrieval(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let p: EvalRetrievalParams = serde_json::from_value(params).map_err(|error| {
            RuntimeError::InvalidInput(format!("knowledge.eval_retrieval: invalid params: {error}"))
        })?;
        if p.query_set.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "knowledge.eval_retrieval: query_set must not be empty".into(),
            ));
        }

        let requested_query_set = PathBuf::from(p.query_set.trim());
        if !requested_query_set.is_absolute() {
            return Err(RuntimeError::InvalidInput(format!(
                "knowledge.eval_retrieval: query_set must be an absolute path, got {}",
                requested_query_set.display()
            )));
        }
        let query_set = tokio::fs::canonicalize(&requested_query_set)
            .await
            .map_err(|error| {
                RuntimeError::InvalidInput(format!(
                    "knowledge.eval_retrieval: cannot resolve query set {}: {error}",
                    requested_query_set.display()
                ))
            })?;
        let persisted_query_set = query_set
            .to_str()
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "knowledge.eval_retrieval: canonical query-set path is not valid UTF-8: {}",
                    query_set.display()
                ))
            })?
            .to_owned();
        let contents = tokio::fs::read_to_string(&query_set)
            .await
            .map_err(|error| {
                RuntimeError::InvalidInput(format!(
                    "knowledge.eval_retrieval: cannot read query set {}: {error}",
                    query_set.display()
                ))
            })?;
        let queries = parse_query_set(&query_set, &contents)?;

        let mut precision_at_5 = 0.0;
        let mut recall_at_5 = 0.0;
        let mut mrr = 0.0;
        for query in &queries {
            let response = Self::search(
                runtime,
                token,
                json!({
                    "query": query.query.as_str(),
                    "type": "atom",
                    "limit": EVAL_K,
                    "include_drafts": true,
                }),
                ann,
            )
            .await?;
            let results = response
                .get("results")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    RuntimeError::Internal(
                        "knowledge.eval_retrieval: search response omitted results".into(),
                    )
                })?;
            let returned_slugs: Vec<String> = results
                .iter()
                .map(|result| {
                    result
                        .get("slug")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            RuntimeError::Internal(
                                "knowledge.eval_retrieval: search result omitted slug".into(),
                            )
                        })
                })
                .collect::<Result<_, _>>()?;
            let metrics = score_query(&query.expected_slugs, &returned_slugs);
            precision_at_5 += metrics.precision_at_5;
            recall_at_5 += metrics.recall_at_5;
            mrr += metrics.mrr;
        }

        let total_queries = queries.len() as i64;
        let denominator = total_queries as f64;
        precision_at_5 /= denominator;
        recall_at_5 /= denominator;
        mrr /= denominator;

        let run_id = Uuid::new_v4().to_string();
        let run_at = Utc::now().timestamp_millis();
        let namespace = token.namespace().as_str().to_string();
        let sql = runtime.sql();
        let mut writer = sql
            .writer()
            .await
            .map_err(|error| sql_err("eval_retrieval writer", error))?;
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_eval_runs \
                      (id, namespace, run_at, query_set, total_queries, precision_at_5, \
                       recall_at_5, mrr, notes) \
                      VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)"
                    .into(),
                params: vec![
                    SqlValue::Text(run_id.clone()),
                    SqlValue::Text(namespace),
                    SqlValue::Integer(run_at),
                    SqlValue::Text(persisted_query_set),
                    SqlValue::Integer(total_queries),
                    SqlValue::Float(precision_at_5),
                    SqlValue::Float(recall_at_5),
                    SqlValue::Float(mrr),
                ],
                label: Some("knowledge.eval_retrieval.persist".into()),
            })
            .await
            .map_err(|error| sql_err("eval_retrieval insert", error))?;

        Ok(json!({
            "run_id": run_id,
            "total_queries": total_queries,
            "precision_at_5": precision_at_5,
            "recall_at_5": recall_at_5,
            "mrr": mrr,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_set_metrics_use_fixed_k_and_ranked_first_hit() {
        let expected = HashSet::from(["alpha".to_string(), "beta".to_string()]);
        let returned = vec!["noise".to_string(), "beta".to_string(), "alpha".to_string()];
        let metrics = score_query(&expected, &returned);
        assert_eq!(metrics.precision_at_5, 0.4);
        assert_eq!(metrics.recall_at_5, 1.0);
        assert_eq!(metrics.mrr, 0.5);
    }

    #[test]
    fn results_beyond_fixed_k_do_not_count() {
        let expected = HashSet::from(["expected".to_string()]);
        let returned = ["a", "b", "c", "d", "e", "expected"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let metrics = score_query(&expected, &returned);
        assert_eq!(metrics.precision_at_5, 0.0);
        assert_eq!(metrics.recall_at_5, 0.0);
        assert_eq!(metrics.mrr, 0.0);
    }

    #[test]
    fn query_set_validation_names_the_invalid_entry() {
        let error = parse_query_set(
            Path::new("fixture.toml"),
            r#"
                [[queries]]
                query = "valid"
                expected_slugs = ["alpha"]

                [[queries]]
                query = " "
                expected_slugs = ["beta"]
            "#,
        )
        .expect_err("blank query must fail");
        let message = error.to_string();
        assert!(message.contains("fixture.toml"));
        assert!(message.contains("[1]"));
    }

    #[test]
    fn empty_query_set_is_a_whole_file_validation_error() {
        for contents in ["", "# no queries yet\n"] {
            let error = parse_query_set(Path::new("fixture.toml"), contents)
                .expect_err("query set without entries must fail");
            let message = error.to_string();
            assert!(message.contains("fixture.toml"));
            assert!(message.contains("contains no queries"));
            assert!(!message.contains("invalid TOML syntax"));
        }
    }

    #[test]
    fn shipped_query_set_is_valid_and_publicly_bounded() {
        let queries = parse_query_set(
            Path::new("eval_set.toml"),
            include_str!("../../tests/fixtures/eval_set.toml"),
        )
        .expect("shipped query set must validate");
        assert_eq!(queries.len(), 10);
    }
}
