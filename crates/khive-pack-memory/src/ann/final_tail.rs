use khive_storage::types::SqlStatement;

use super::{parse_final_tail_rows, sanitize_model_key, FinalTail};

/// Runs the single fresh-tail snapshot on a caller-supplied reader. Serving
/// paths use this seam while coordinating the ADR-118 registry guard.
pub(super) async fn fetch_final_tail_on(
    reader: &mut dyn khive_storage::SqlReader,
    model: &str,
    s: u64,
    live_threshold: Option<f64>,
) -> Result<FinalTail, String> {
    let rows = reader
        .query_all(final_tail_statement(model, s, live_threshold))
        .await
        .map_err(|e| e.to_string())?;
    parse_final_tail_rows(&rows, model, s)
}

pub(super) fn final_tail_statement(
    model: &str,
    s: u64,
    live_threshold: Option<f64>,
) -> SqlStatement {
    let table_name = format!("vec_{}", sanitize_model_key(model));
    super::memory_corpus().final_tail(
        &table_name,
        model,
        s as i64,
        live_threshold,
        khive_retrieval::ann::corpus::TailFloor::CallerGuarded,
        "memory_ann_fresh_tail_snapshot",
    )
}
