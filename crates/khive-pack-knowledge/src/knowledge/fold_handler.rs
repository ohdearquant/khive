//! Fold handler: greedy knapsack selection over scored candidates.

use serde_json::{json, Value};

use khive_fold::{GreedySelector, Selector, SelectorInput, SelectorWeights};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::schema::{FoldCandidate, FoldParams};
use super::util::deser;
use super::KnowledgeHandlers;

impl KnowledgeHandlers {
    pub(crate) async fn fold(
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: FoldParams = deser(params)?;

        if let Some(min_score) = p.min_score {
            if !min_score.is_finite() {
                return Err(RuntimeError::InvalidInput(
                    "min_score must be a finite number".into(),
                ));
            }
        }
        if let Some(bias) = p.diversity_bias {
            if !bias.is_finite() {
                return Err(RuntimeError::InvalidInput(
                    "diversity_bias must be a finite number".into(),
                ));
            }
        }
        if let Some(ew) = p.epistemic_weight {
            if !ew.is_finite() {
                return Err(RuntimeError::InvalidInput(
                    "epistemic_weight must be a finite number".into(),
                ));
            }
        }
        if let Some(ref cw) = p.category_weights {
            for (k, v) in cw {
                if !v.is_finite() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "category_weights[{k:?}] must be a finite number"
                    )));
                }
            }
        }
        for (i, c) in p.candidates.iter().enumerate() {
            if !c.score.is_finite() {
                return Err(RuntimeError::InvalidInput(format!(
                    "candidates[{i}].score must be a finite number"
                )));
            }
            if let Some(ig) = c.information_gain {
                if !ig.is_finite() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "candidates[{i}].information_gain must be a finite number"
                    )));
                }
            }
        }

        if p.candidates.is_empty() {
            return Ok(json!({
                "selected": [],
                "total_size": 0,
                "budget": p.budget,
                "selected_count": 0,
            }));
        }

        let inputs: Vec<SelectorInput<FoldCandidate>> = p
            .candidates
            .iter()
            .cloned()
            .map(|c| SelectorInput {
                id: c.id.clone(),
                score: c.score,
                size: c.size,
                category: c.category.clone(),
                information_gain: c.information_gain,
                content: c,
            })
            .collect();

        let weights = SelectorWeights {
            min_score: p.min_score.unwrap_or(0.0),
            category_weights: p.category_weights.unwrap_or_default().into_iter().collect(),
            diversity_bias: p.diversity_bias.unwrap_or(0.0),
            epistemic_weight: p.epistemic_weight.unwrap_or(0.0),
        };

        let output = GreedySelector
            .select(inputs, p.budget, &weights)
            .map_err(|e| RuntimeError::Internal(format!("fold selector: {e}")))?;

        let selected: Vec<Value> = output
            .selected
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "score": item.score,
                    "size": item.size,
                    "content": item.content.content,
                    "category": item.content.category,
                })
            })
            .collect();

        Ok(json!({
            "selected": selected,
            "total_size": output.total_size,
            "budget": p.budget,
            "selected_count": output.selected.len(),
        }))
    }
}
