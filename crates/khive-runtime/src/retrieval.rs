//! Retrieval operations: local embedding generation and hybrid search with RRF fusion.

use std::collections::{HashMap, HashSet};

use khive_db::pool::NoteSearchFallbackCause;
use lattice_embed::{EmbeddingModel, EmbeddingRole, EmbeddingService, TokenCount, MAX_TEXT_BYTES};
use uuid::Uuid;

use crate::config::{parse_embedding_model_alias, sanitize_key};
use crate::curation::note_fts_document;
use crate::embedder_registry::with_embedding_admission;
use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};
use khive_retrieval::hybrid::{combine_leg_first_appearance, fuse_labelled, HitLabel};
use khive_score::DeterministicScore;
use khive_storage::types::{
    PageRequest, TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, VectorRecord,
    VectorSearchHit,
};
use khive_storage::ContentRef;
use khive_storage::EntityFilter;
use khive_types::SubstrateKind;

/// The model value a custom provider's query-role method receives. A custom
/// provider has no built-in model, but the service trait takes one, and the
/// trait's default query method prepends that model's query instruction. This
/// model has neither a query nor a document instruction, so a custom query
/// keeps its original text whether or not the provider overrides the method.
const CUSTOM_QUERY_ROLE_MODEL: EmbeddingModel = EmbeddingModel::AllMiniLmL6V2;

pub use khive_retrieval::{
    HybridSearchOutcome, RankScoreKind, SearchHit, SearchSignals, SearchSource,
};

mod single_engine;

#[cfg(test)]
mod request_stop_tests;

#[cfg(test)]
mod token_report_tests;

/// Bounds provider input and per-page outcome memory while amortizing model setup.
pub(crate) const EMBEDDING_BATCH_PAGE_SIZE: usize = 256;

// Fault-injection flag for backfill reader errors (test / `fault-injection` builds only).
#[cfg(any(test, feature = "fault-injection"))]
std::thread_local! {
    static BACKFILL_READER_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the backfill reader fault injection: the next `backfill_missing_embeddings`
/// call substitutes a `StorageError::Pool` for `sql.reader().await`'s result, then
/// resets the flag. The injected error passes through the same
/// `map_err(RuntimeError::Storage)?` path as a real reader failure, exercising the
/// fail-closed guard rather than bypassing it.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_backfill_reader_fail() {
    BACKFILL_READER_FAIL.with(|c| c.set(true));
}

/// RRF constant. Controls how strongly top ranks dominate.
///
/// The paper's k=60 over-compresses scores at KG scale (tens–thousands of
/// entities): rank 1 ≈ 0.016, rank 10 ≈ 0.014, spread ≈ 0.002. k=10 gives
/// rank 1 ≈ 0.091, rank 10 ≈ 0.050, spread ≈ 0.041 — 20× better discrimination,
/// which dedup-before-create needs at graph sizes of 50–2700 entities.
const RRF_K: usize = 10;

/// Candidates pulled per path before fusion. Higher = better recall, more work.
const CANDIDATE_MULTIPLIER: u32 = 4;

/// Advisory emitted by write verbs when only the embedding input was bounded.
pub const EMBEDDING_INPUT_TRUNCATED_WARNING: &str =
    "embedding input was truncated to the embedder maximum; full content was stored unchanged";

/// Outcome for one document embedding. The exact-input fingerprint is present
/// only when the resolved provider has the runtime-audited preparation path.
#[derive(Clone, Debug)]
pub struct DocumentEmbeddingOutcome {
    pub model_name: String,
    pub vector: Vec<f32>,
    /// Digest of the exact prepared input, or `None` for an unaudited provider.
    pub prepared_text_fingerprint: Option<ContentRef>,
    pub source_bytes: usize,
    pub embedded_bytes: usize,
    /// Counts for the byte-bounded input, including role instructions and special tokens.
    /// None means the provider does not support token reporting.
    pub provider_token_count: Option<TokenCount>,
    pub truncated: bool,
}

// The provider's token count describes the byte-bounded input; it does not
// estimate tokens discarded by the earlier byte limit.
fn document_vector_or_refusal(outcome: DocumentEmbeddingOutcome) -> RuntimeResult<Vec<f32>> {
    if !outcome.truncated {
        return Ok(outcome.vector);
    }
    let token_count = outcome.provider_token_count.filter(|count| count.truncated());
    let byte_cut = outcome.source_bytes > outcome.embedded_bytes;
    let detail = match (byte_cut, token_count) {
        (true, Some(count)) => format!(
            "from {} to {} bytes and from {} to {} provider tokens for the byte-bounded input",
            outcome.source_bytes, outcome.embedded_bytes, count.before_truncation, count.embedded
        ),
        (true, None) => format!("from {} to {} bytes", outcome.source_bytes, outcome.embedded_bytes),
        (false, Some(count)) => format!(
            "from {} to {} provider tokens", count.before_truncation, count.embedded
        ),
        (false, None) => "by an unspecified input limit".to_string(),
    };
    Err(RuntimeError::InvalidInput(format!(
        "embedding input truncated {detail}; use embed_document_outcome to inspect the bounded vector"
    )))
}

/// Aggregate truncation accounting for one logical write or reindex batch.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct EmbeddingTruncationReport {
    pub truncated: u64,
    pub discarded_bytes: u64,
    #[serde(default, skip_serializing_if = "counter_is_zero")]
    pub discarded_tokens: u64,
    #[serde(default, skip_serializing_if = "counter_is_zero")]
    pub token_reporting_unavailable: u64,
}

fn counter_is_zero(value: &u64) -> bool {
    *value == 0
}

impl EmbeddingTruncationReport {
    pub fn observe(&mut self, outcome: &DocumentEmbeddingOutcome) {
        if outcome.truncated {
            self.truncated = self.truncated.saturating_add(1);
            self.discarded_bytes = self
                .discarded_bytes
                .saturating_add(outcome.source_bytes.saturating_sub(outcome.embedded_bytes) as u64);
        }
        if let Some(count) = outcome.provider_token_count {
            self.discarded_tokens = self
                .discarded_tokens
                .saturating_add(count.before_truncation.saturating_sub(count.embedded) as u64);
        } else {
            self.token_reporting_unavailable = self.token_reporting_unavailable.saturating_add(1);
        }
    }

    #[must_use]
    pub const fn any_truncated(&self) -> bool {
        self.truncated > 0
    }

    /// Add another batch's counters without wrapping on overflow.
    pub fn merge(&mut self, other: Self) {
        self.truncated = self.truncated.saturating_add(other.truncated);
        self.discarded_bytes = self.discarded_bytes.saturating_add(other.discarded_bytes);
        self.discarded_tokens = self.discarded_tokens.saturating_add(other.discarded_tokens);
        self.token_reporting_unavailable = self
            .token_reporting_unavailable
            .saturating_add(other.token_reporting_unavailable);
    }
}

struct DocumentEmbeddings {
    vectors: Vec<Vec<f32>>,
    token_counts: Option<Vec<TokenCount>>,
}

async fn document_embeddings(
    service: &dyn EmbeddingService,
    texts: &[String],
    model: EmbeddingModel,
) -> RuntimeResult<DocumentEmbeddings> {
    let result = with_embedding_admission(async {
        match service
            .embed_with_report(texts, model, EmbeddingRole::Passage)
            .await
        {
            Ok(report) => Ok(DocumentEmbeddings {
                vectors: report.embeddings,
                token_counts: Some(report.token_counts),
            }),
            Err(lattice_embed::EmbedError::Unsupported(_)) => service
                .embed_passage(texts, model)
                .await
                .map(|vectors| DocumentEmbeddings {
                    vectors,
                    token_counts: None,
                }),
            Err(error) => Err(error),
        }
    })
    .await?;
    if let Some(counts) = &result.token_counts {
        if result.vectors.len() != texts.len() || counts.len() != texts.len() {
            return Err(RuntimeError::Internal(format!(
                "embed_with_report returned {} vectors and {} token counts for {} inputs",
                result.vectors.len(),
                counts.len(),
                texts.len(),
            )));
        }
        if counts
            .iter()
            .any(|count| count.embedded > count.before_truncation)
        {
            return Err(RuntimeError::Internal(
                "embed_with_report returned an embedded token count above the input count".into(),
            ));
        }
    }
    Ok(result)
}

/// Maximum document bytes accepted before the embedding service adds its model prefix.
pub fn document_embedding_budget(model_name: &str) -> usize {
    parse_embedding_model_alias(model_name)
        .and_then(|model| model.document_instruction())
        .map_or(MAX_TEXT_BYTES, |prefix| {
            MAX_TEXT_BYTES.saturating_sub(prefix.len())
        })
}

/// Return the longest UTF-8-safe prefix of `text` within `max_bytes` and whether it was shortened.
pub fn bounded_embedding_input(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }

    let end = text
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    (&text[..end], true)
}

fn prepared_document_fingerprint(text: &str, model: EmbeddingModel) -> ContentRef {
    match model.document_instruction() {
        Some(prefix) => VectorRecord::fingerprint_text(&format!("{prefix}{text}")),
        None => VectorRecord::fingerprint_text(text),
    }
}

impl KhiveRuntime {
    fn require_default_embedder(&self) -> RuntimeResult<&str> {
        let model_name = self.default_embedder_name();
        if model_name.is_empty() {
            return Err(RuntimeError::Unconfigured("embedding_model".into()));
        }
        Ok(model_name)
    }

    /// Generate an embedding vector for `text` using the configured default model.
    ///
    /// First call lazily loads model weights (cold start cost). Subsequent calls reuse them.
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    pub async fn embed(&self, text: &str) -> RuntimeResult<Vec<f32>> {
        let model_name = self.require_default_embedder()?;
        self.embed_with_model(model_name, text).await
    }

    /// Generate an embedding vector for `text` using the named model.
    ///
    /// Accepts both built-in lattice model names/aliases and custom provider
    /// names registered via [`KhiveRuntime::register_embedder`]. For lattice
    /// models the resolved `EmbeddingModel` enum is forwarded to `embed_one`
    /// so the service can select the correct model variant. For custom
    /// providers, `embed_one` is called with `EmbeddingModel::default()`
    /// because custom services are expected to ignore the enum argument (they
    /// own a single model implicitly).
    ///
    /// Applies no instruction prefix (generic role). Use
    /// [`Self::embed_document_with_model_outcome`] / [`Self::embed_query_with_model`] for
    /// instruction-tuned models where the asymmetric prefix matters.
    ///
    /// Returns `UnknownModel` if `model_name` is not in the embedder registry.
    pub async fn embed_with_model(&self, model_name: &str, text: &str) -> RuntimeResult<Vec<f32>> {
        let model = parse_embedding_model_alias(model_name);
        let service = self.embedder(model_name).await?;
        let emb_model = model.unwrap_or_default();
        // Issued-at-dispatch: count before the provider await so a call that
        // was handed to the provider is counted even if this task is aborted
        // while parked on the await (drain_embed_join_set cancellation path).
        crate::usage::count(crate::usage::UsageUnit::EmbedCalls, 1);
        let out = with_embedding_admission(service.embed_one(text, emb_model)).await;
        out
    }

    /// Embed a document/passage for indexing using the named model.
    ///
    /// Applies `EmbeddingService::embed_with_report` with the passage role, which prepends the model's
    /// `document_instruction()` prefix when defined (e.g. `"passage: "` for
    /// multilingual-e5). For models with no document prefix (MiniLM, BGE) this
    /// is identical to [`Self::embed_with_model`].
    ///
    /// Use this for all index/store/backfill paths so that instruction-tuned
    /// models produce passage-side vectors.
    ///
    /// **Reindex caveat**: switching from an unprefixed model (or a model with no
    /// `document_instruction`) to an instruction-tuned model changes the vector
    /// representation. Vectors stored under the old scheme are not comparable to
    /// newly prefixed vectors. Operators must trigger a full reindex
    /// (`knowledge.index(rebuild_ann=true)` / `kkernel reindex`) after changing
    /// the embedding model config.
    ///
    /// Returns `UnknownModel` if `model_name` is not registered.
    pub async fn embed_document_with_model_outcome(
        &self,
        model_name: &str,
        text: &str,
    ) -> RuntimeResult<DocumentEmbeddingOutcome> {
        self.embed_document_with_model_outcome_inner(None, model_name, text)
            .await
    }

    pub(crate) async fn embed_document_with_model_outcome_for_token(
        &self,
        token: &NamespaceToken,
        model_name: &str,
        text: &str,
    ) -> RuntimeResult<DocumentEmbeddingOutcome> {
        self.embed_document_with_model_outcome_inner(Some(token), model_name, text)
            .await
    }

    async fn embed_document_with_model_outcome_inner(
        &self,
        token: Option<&NamespaceToken>,
        model_name: &str,
        text: &str,
    ) -> RuntimeResult<DocumentEmbeddingOutcome> {
        let model = parse_embedding_model_alias(model_name);
        let (service, audited_document_preparation) = self
            .embedder_with_input_attestation(model_name, token)
            .await?;
        let emb_model = model.unwrap_or_default();
        let source_bytes = text.len();
        let (text, truncated) =
            bounded_embedding_input(text, document_embedding_budget(model_name));
        let embedded_bytes = text.len();
        // Issued-at-dispatch: counted before the await — see embed_with_model.
        crate::usage::count(crate::usage::UsageUnit::EmbedCalls, 1);
        let embeddings =
            document_embeddings(service.as_ref(), &[text.to_string()], emb_model).await?;
        let provider_token_count = embeddings.token_counts.map(|counts| counts[0]);
        let mut vectors = embeddings.vectors;
        if vectors.len() != 1 {
            return Err(RuntimeError::Internal(format!(
                "embed_passage returned {} vectors for 1 input",
                vectors.len()
            )));
        }
        let out = vectors.pop().expect("checked len == 1 above");
        Ok(DocumentEmbeddingOutcome {
            model_name: model_name.to_owned(),
            vector: out,
            prepared_text_fingerprint: audited_document_preparation
                .then(|| prepared_document_fingerprint(text, emb_model)),
            source_bytes,
            embedded_bytes,
            provider_token_count,
            truncated: truncated || provider_token_count.is_some_and(|count| count.truncated()),
        })
    }

    /// Embed a query string for retrieval using the named model.
    ///
    /// Applies the pre-0.6 query-role behavior. E5 and Qwen models retain
    /// their query instructions and BGE receives the original unprefixed query
    /// text. A custom provider receives the original text through its
    /// query-role method and owns its own query preparation; the trait's
    /// default query method adds no instruction to it.
    ///
    /// Use this for all search/recall/suggest query embedding paths so that
    /// instruction-tuned models land in the correct side of their retrieval
    /// space.
    ///
    /// Returns `UnknownModel` if `model_name` is not registered.
    pub async fn embed_query_with_model(
        &self,
        model_name: &str,
        text: &str,
    ) -> RuntimeResult<Vec<f32>> {
        self.embed_query_with_model_inner(None, model_name, text)
            .await
    }

    pub(crate) async fn embed_query_with_model_for_token(
        &self,
        token: &NamespaceToken,
        model_name: &str,
        text: &str,
    ) -> RuntimeResult<Vec<f32>> {
        self.embed_query_with_model_inner(Some(token), model_name, text)
            .await
    }

    async fn embed_query_with_model_inner(
        &self,
        token: Option<&NamespaceToken>,
        model_name: &str,
        text: &str,
    ) -> RuntimeResult<Vec<f32>> {
        let _phase = khive_storage::request_context::enter_search_phase(
            khive_storage::request_context::SearchPhase::VectorEmbedding,
        );
        let model = parse_embedding_model_alias(model_name);
        let service = match token {
            Some(token) => self.embedder_with_token(token, model_name).await?,
            None => self.embedder(model_name).await?,
        };
        let texts = [text.to_string()];
        let emb_model = model.unwrap_or(CUSTOM_QUERY_ROLE_MODEL);
        // Issued-at-dispatch: counted before the await — see embed_with_model.
        crate::usage::count(crate::usage::UsageUnit::EmbedCalls, 1);
        let embeddings = match model {
            Some(
                EmbeddingModel::BgeSmallEnV15
                | EmbeddingModel::BgeBaseEnV15
                | EmbeddingModel::BgeLargeEnV15,
            ) => with_embedding_admission(service.embed(&texts, emb_model)).await,
            _ => with_embedding_admission(service.embed_query(&texts, emb_model)).await,
        };
        let out = embeddings?
            .into_iter()
            .next()
            .ok_or_else(|| RuntimeError::Internal("embed_query returned empty vec".into()))?;
        Ok(out)
    }

    /// Embed a document for indexing using the configured default model.
    ///
    /// Delegates to [`Self::embed_document_outcome`]. Use for entity/note
    /// create and reindex paths.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    /// Returns an error if the input is bounded; use the outcome method to
    /// inspect the vector together with the truncation metadata.
    pub async fn embed_document(&self, text: &str) -> RuntimeResult<Vec<f32>> {
        let outcome = self.embed_document_outcome(text).await?;
        document_vector_or_refusal(outcome)
    }

    /// Embed a document with the default model and retain input-bounding metadata.
    pub async fn embed_document_outcome(
        &self,
        text: &str,
    ) -> RuntimeResult<DocumentEmbeddingOutcome> {
        let model_name = self.require_default_embedder()?;
        self.embed_document_with_model_outcome(model_name, text)
            .await
    }

    /// Embed a query for retrieval using the configured default model.
    ///
    /// Delegates to [`Self::embed_query_with_model`]. Use for vector search and
    /// hybrid search query paths.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    pub async fn embed_query(&self, text: &str) -> RuntimeResult<Vec<f32>> {
        let model_name = self.require_default_embedder()?;
        self.embed_query_with_model(model_name, text).await
    }

    async fn embed_query_for_token(
        &self,
        token: &NamespaceToken,
        text: &str,
    ) -> RuntimeResult<Vec<f32>> {
        let model_name = self.require_default_embedder()?;
        self.embed_query_with_model_for_token(token, model_name, text)
            .await
    }

    /// Generate embeddings for multiple texts in one call using the configured default model.
    ///
    /// Delegates to the cached `EmbeddingService::embed`, so repeated texts within
    /// and across calls benefit from the runtime-level LRU cache.
    ///
    /// Returns an empty vec for empty input without hitting the embedding service.
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    pub async fn embed_batch(&self, texts: &[String]) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model_name = self.require_default_embedder()?;
        self.embed_batch_with_model(model_name, texts).await
    }

    /// Generate embeddings for multiple texts using the named model.
    ///
    /// Accepts lattice model names/aliases and custom provider names.
    /// Returns `UnknownModel` if `model_name` is not in the embedder registry.
    pub async fn embed_batch_with_model(
        &self,
        model_name: &str,
        texts: &[String],
    ) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model = parse_embedding_model_alias(model_name);
        let service = self.embedder(model_name).await?;
        let emb_model = model.unwrap_or_default();
        let out = with_embedding_admission(service.embed(texts, emb_model)).await;
        crate::usage::count(crate::usage::UsageUnit::EmbedCalls, texts.len() as u64);
        out
    }

    /// Embed a batch of documents for indexing using the named model.
    ///
    /// Applies passage-role embedding with token reporting when supported. Use for all bulk
    /// index/backfill/reindex operations to apply the passage-side prefix.
    /// A mixed batch is bounded into one ordered owned batch so truncation never
    /// fragments one provider batch into sequential singleton inference calls.
    ///
    /// **Reindex caveat**: see [`Self::embed_document_with_model_outcome`] — the same
    /// incomparability applies to batch-indexed vectors when switching models.
    ///
    /// Returns `UnknownModel` if `model_name` is not registered.
    /// Returns an error when any input is bounded; use the outcomes method to
    /// retain the bounded vectors and per-document byte counts.
    pub async fn embed_document_batch_with_model(
        &self,
        model_name: &str,
        texts: &[String],
    ) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let outcomes = self
            .embed_document_batch_with_model_outcomes(model_name, texts)
            .await?;
        let mut report = EmbeddingTruncationReport::default();
        for outcome in &outcomes {
            report.observe(outcome);
        }
        if report.any_truncated() {
            return Err(RuntimeError::InvalidInput(format!(
                "embedding input truncated for {} documents ({} discarded bytes, {} discarded tokens); use embed_document_batch_with_model_outcomes to inspect the bounded vectors",
                report.truncated, report.discarded_bytes, report.discarded_tokens
            )));
        }
        Ok(outcomes.into_iter().map(|outcome| outcome.vector).collect())
    }

    pub async fn embed_document_batch_with_model_outcomes(
        &self,
        model_name: &str,
        texts: &[String],
    ) -> RuntimeResult<Vec<DocumentEmbeddingOutcome>> {
        self.embed_document_batch_with_model_outcomes_inner(None, model_name, texts)
            .await
    }

    pub(crate) async fn embed_document_batch_with_model_outcomes_for_token(
        &self,
        token: &NamespaceToken,
        model_name: &str,
        texts: &[String],
    ) -> RuntimeResult<Vec<DocumentEmbeddingOutcome>> {
        self.embed_document_batch_with_model_outcomes_inner(Some(token), model_name, texts)
            .await
    }

    async fn embed_document_batch_with_model_outcomes_inner(
        &self,
        token: Option<&NamespaceToken>,
        model_name: &str,
        texts: &[String],
    ) -> RuntimeResult<Vec<DocumentEmbeddingOutcome>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model = parse_embedding_model_alias(model_name);
        let (service, audited_document_preparation) = self
            .embedder_with_input_attestation(model_name, token)
            .await?;
        let emb_model = model.unwrap_or_default();
        let budget = document_embedding_budget(model_name);
        if token.is_some() {
            // Atomic preparation records issued work even if its task is cancelled.
            crate::usage::count(crate::usage::UsageUnit::EmbedCalls, texts.len() as u64);
        }
        let out = if texts.iter().all(|text| text.len() <= budget) {
            document_embeddings(service.as_ref(), texts, emb_model).await
        } else {
            let bounded_texts: Vec<String> = texts
                .iter()
                .map(|text| bounded_embedding_input(text, budget).0.to_owned())
                .collect();
            document_embeddings(service.as_ref(), &bounded_texts, emb_model).await
        };
        if token.is_none() {
            crate::usage::count(crate::usage::UsageUnit::EmbedCalls, texts.len() as u64);
        }
        let embeddings = out?;
        let vectors = embeddings.vectors;
        if vectors.len() != texts.len() {
            return Err(RuntimeError::Internal(format!(
                "embed_passage returned {} vectors for {} inputs",
                vectors.len(),
                texts.len()
            )));
        }
        Ok(texts
            .iter()
            .zip(vectors)
            .enumerate()
            .map(|(index, (text, vector))| {
                let (bounded, truncated) = bounded_embedding_input(text, budget);
                let provider_token_count =
                    embeddings.token_counts.as_ref().map(|counts| counts[index]);
                DocumentEmbeddingOutcome {
                    model_name: model_name.to_owned(),
                    vector,
                    prepared_text_fingerprint: audited_document_preparation
                        .then(|| prepared_document_fingerprint(bounded, emb_model)),
                    source_bytes: text.len(),
                    embedded_bytes: bounded.len(),
                    provider_token_count,
                    truncated: truncated
                        || provider_token_count.is_some_and(|count| count.truncated()),
                }
            })
            .collect())
    }

    /// Embed a batch of documents for indexing using the configured default model.
    ///
    /// Convenience delegate to [`Self::embed_document_batch_with_model`]. Use for
    /// bulk knowledge-atom and section indexing paths.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    /// Returns an error when any input is bounded; use
    /// [`Self::embed_document_batch_outcomes`] to retain the bounded vectors.
    pub async fn embed_document_batch(&self, texts: &[String]) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model_name = self.require_default_embedder()?;
        self.embed_document_batch_with_model(model_name, texts)
            .await
    }

    /// Embed documents with the default model and retain actual input-bounding metadata.
    pub async fn embed_document_batch_outcomes(
        &self,
        texts: &[String],
    ) -> RuntimeResult<Vec<DocumentEmbeddingOutcome>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model_name = self.require_default_embedder()?;
        self.embed_document_batch_with_model_outcomes(model_name, texts)
            .await
    }

    /// Embed a batch of queries for retrieval using the named model.
    ///
    /// Applies the same pre-0.6 query-role compatibility behavior as
    /// [`Self::embed_query_with_model`].
    ///
    /// Returns `UnknownModel` if `model_name` is not registered.
    pub async fn embed_query_batch_with_model(
        &self,
        model_name: &str,
        texts: &[String],
    ) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model = parse_embedding_model_alias(model_name);
        let service = self.embedder(model_name).await?;
        let emb_model = model.unwrap_or(CUSTOM_QUERY_ROLE_MODEL);
        let out = match model {
            Some(
                EmbeddingModel::BgeSmallEnV15
                | EmbeddingModel::BgeBaseEnV15
                | EmbeddingModel::BgeLargeEnV15,
            ) => with_embedding_admission(service.embed(texts, emb_model)).await,
            _ => with_embedding_admission(service.embed_query(texts, emb_model)).await,
        };
        crate::usage::count(crate::usage::UsageUnit::EmbedCalls, texts.len() as u64);
        out
    }

    /// The note-search vector leg uses the pack-owned graph when that model
    /// has an installed, consumer-protected bridge. A missing model, backend
    /// provider, or eligible graph takes the existing exact sqlite-vec route.
    pub(crate) async fn note_search_vector_search(
        &self,
        token: &NamespaceToken,
        query_embedding: Option<Vec<f32>>,
        query_text: &str,
        top_k: u32,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let embedding = match query_embedding {
            Some(embedding) => embedding,
            None => self.embed_query_for_token(token, query_text).await?,
        };
        let model = self.default_embedder_name();
        let fallback_cause = if model.is_empty() {
            NoteSearchFallbackCause::NoModel
        } else if let Some(provider) = self.note_search_ann_provider()? {
            if let Some(hits) = provider.search(token, model, &embedding, top_k).await? {
                crate::note_search_ann::record_ann_route(self.backend().pool());
                crate::usage::count(crate::usage::UsageUnit::VectorPasses, 1);
                return Ok(hits);
            }
            NoteSearchFallbackCause::NoEligibleGraph
        } else {
            NoteSearchFallbackCause::NoProvider
        };
        crate::note_search_ann::record_fallback_route(self.backend().pool(), fallback_cause);
        self.vector_search(
            token,
            Some(embedding),
            None,
            top_k,
            Some(SubstrateKind::Note),
        )
        .await
    }

    /// Hybrid search: text (FTS5) + vector retrieval fused via Reciprocal Rank Fusion.
    ///
    /// - Always performs text search over `query_text`.
    /// - If `query_vector` is `Some`, also performs vector search and fuses both lists.
    /// - If `None`, returns text-only results — no vector store needed.
    /// - If `entity_kind` is `Some`, the alive-set query filters to that kind.
    ///   The text/vector candidate pools are unfiltered up front; the kind
    ///   filter applies at the alive-check stage where we already fetch each
    ///   candidate to confirm it isn't soft-deleted.
    /// - `tags_any`: when non-empty, only entities that have at least one of these
    ///   tags (case-insensitive) survive the alive-set intersection. Applied BEFORE
    ///   truncation so matches ranked beyond `limit` in the raw fusion are not lost.
    /// - `properties_filter`: when `Some`, only entities whose properties are a
    ///   superset of the given JSON object survive. Applied BEFORE truncation.
    ///
    /// `limit` caps the final returned list; internally pulls `limit * 4` candidates per path.
    ///
    /// # Cross-namespace visibility (entity search — text leg: visible set; vector leg: primary)
    ///
    /// The two legs of entity search scope namespaces differently.
    ///
    /// **Text leg.** Entity full-text search is one shared table (`fts_entities`)
    /// with a `namespace` column. The token's whole visible set (`visible_ns`) is
    /// forwarded in `TextFilter.namespaces`, which the store applies as a
    /// `namespace IN (...)` predicate, so one query returns text hits from every
    /// visible namespace.
    ///
    /// **Vector leg.** The vector leg runs only when a query vector is supplied or
    /// an embedding model is configured. It is an exact sqlite-vec search
    /// (brute-force cosine, as in `knn`); no ANN index is involved. It issues a
    /// single request scoped to the primary namespace, because a
    /// `VectorSearchRequest` carries one namespace. Searching the visible set
    /// would take one request per namespace and a merge of the per-namespace
    /// lists; that fanout is not implemented for entities, so entity vector hits
    /// come from the primary namespace only.
    ///
    /// The fused candidates are then checked against the entity store with the
    /// visible set, so an entity from an extra visible namespace is returned when
    /// the text leg matched it, and is not returned on a vector-only match.
    ///
    /// This differs from `memory.recall`, whose vector leg already searches every
    /// visible namespace.
    ///
    /// Callers can also read any visible entity directly via `get_entity` /
    /// `resolve`.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        entity_kind: Option<&str>,
        entity_type: Option<&str>,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
    ) -> RuntimeResult<Vec<SearchHit>> {
        self.hybrid_search_with_text_mode(
            token,
            query_text,
            query_vector,
            limit,
            entity_kind,
            entity_type,
            tags_any,
            properties_filter,
            TextQueryMode::Plain,
        )
        .await
    }

    /// Hybrid search with an explicit lexical mode for the text arm.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search_with_text_mode(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        entity_kind: Option<&str>,
        entity_type: Option<&str>,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
        text_mode: TextQueryMode,
    ) -> RuntimeResult<Vec<SearchHit>> {
        let (hits, _vector_error) = self
            .hybrid_search_inner(
                token,
                query_text,
                query_vector,
                limit,
                entity_kind,
                entity_type,
                tags_any,
                properties_filter,
                text_mode,
                None,
                false,
                None,
            )
            .await?;
        Ok(hits)
    }

    /// Hybrid search over several entity kinds that issues the vector query once.
    ///
    /// Returns one list per entry of `entity_kinds`, in that order. Each list is what
    /// [`Self::hybrid_search`] returns for that kind with the same `limit`: the text stage
    /// is filtered to the kind and keeps its own `limit * 4` budget, and fusion, the kind
    /// filter and the cut to `limit` run per kind. Only the vector stage is shared, because
    /// it takes no entity kind: the single query is the one every per-kind search would have
    /// issued. When `query_vector` is `None` and an embedding model is configured, the query
    /// text is embedded once instead of once per kind. No entity-type, tag or property
    /// filter is set. An empty `entity_kinds` returns no lists and runs no query.
    pub async fn hybrid_search_each_kind(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        entity_kinds: &[&str],
    ) -> RuntimeResult<Vec<Vec<SearchHit>>> {
        if entity_kinds.is_empty() {
            return Ok(Vec::new());
        }
        let candidates = limit.saturating_mul(CANDIDATE_MULTIPLIER).max(limit);
        let (vector_hits, _vector_error) = self
            .hybrid_vector_stage(token, query_text, query_vector, candidates, None, false)
            .await?;
        let mut per_kind = Vec::with_capacity(entity_kinds.len());
        for &kind in entity_kinds {
            let (hits, _vector_error) = self
                .hybrid_search_inner(
                    token,
                    query_text,
                    None,
                    limit,
                    Some(kind),
                    None,
                    &[],
                    None,
                    TextQueryMode::Plain,
                    None,
                    false,
                    Some(vector_hits.clone()),
                )
                .await?;
            per_kind.push(hits);
        }
        Ok(per_kind)
    }

    /// `vector_similarity_floor` is a cosine-similarity value in `[-1.0,
    /// 1.0]`, matching both the canonical vector-store score contract and the
    /// scale documented on the `resolve` verb and on
    /// `SEARCH_VECTOR_SIMILARITY_FLOOR`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn hybrid_search_with_vector_similarity_floor(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        entity_kind: Option<&str>,
        entity_type: Option<&str>,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
        vector_similarity_floor: f64,
    ) -> RuntimeResult<Vec<SearchHit>> {
        let (hits, _vector_error) = self
            .hybrid_search_inner(
                token,
                query_text,
                query_vector,
                limit,
                entity_kind,
                entity_type,
                tags_any,
                properties_filter,
                TextQueryMode::Plain,
                Some(vector_similarity_floor),
                false,
                None,
            )
            .await?;
        Ok(hits)
    }

    /// Coordinator fan-out variant of [`Self::hybrid_search`]: the text arm
    /// still fails loud (propagated through `hybrid_search_inner`'s
    /// `tolerate_vector_error=false` semantics for that leg), but an
    /// ordinary vector-arm failure after a successful text leg is captured
    /// instead of discarding the text hits. Request cancellation and the outer
    /// deadline remain fatal. Reserved for
    /// `SubstrateCoordinator::fan_out_search_with_visibility` — every other
    /// caller keeps the fail-loud [`Self::hybrid_search`] contract, so a
    /// single backend's vector-store outage does not misreport that
    /// backend's text arm as failed too.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search_outcome(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        limit: u32,
        entity_kind: Option<&str>,
        entity_type: Option<&str>,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
    ) -> RuntimeResult<HybridSearchOutcome> {
        self.hybrid_search_outcome_with_text_mode(
            token,
            query_text,
            limit,
            entity_kind,
            entity_type,
            tags_any,
            properties_filter,
            TextQueryMode::Plain,
        )
        .await
    }

    /// Coordinator variant with an explicit lexical mode for the text arm.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search_outcome_with_text_mode(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        limit: u32,
        entity_kind: Option<&str>,
        entity_type: Option<&str>,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
        text_mode: TextQueryMode,
    ) -> RuntimeResult<HybridSearchOutcome> {
        let (hits, vector_error) = self
            .hybrid_search_inner(
                token,
                query_text,
                None,
                limit,
                entity_kind,
                entity_type,
                tags_any,
                properties_filter,
                text_mode,
                None,
                true,
                None,
            )
            .await?;
        Ok(HybridSearchOutcome { hits, vector_error })
    }

    /// `vector_pool`, when `Some`, supplies the vector stage's hits and the stage does not
    /// run; `None` runs it. The stage takes no entity kind, so one pool serves every kind.
    #[allow(clippy::too_many_arguments)]
    async fn hybrid_search_inner(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        entity_kind: Option<&str>,
        entity_type: Option<&str>,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
        text_mode: TextQueryMode,
        vector_similarity_floor: Option<f64>,
        tolerate_vector_error: bool,
        vector_pool: Option<Vec<VectorSearchHit>>,
    ) -> RuntimeResult<(Vec<SearchHit>, Option<String>)> {
        khive_storage::await_request_read_phase("hybrid_search", async {
            let candidates = limit.saturating_mul(CANDIDATE_MULTIPLIER).max(limit);

            let visible_ns: Vec<String> = token
                .visible_namespaces()
                .iter()
                .map(|ns| ns.as_str().to_owned())
                .collect();
            // sanitize_fts5_query strips known-unsafe FTS5 metacharacters up front, but if
            // the lexical leg still errors at runtime on residual punctuation the sanitizer
            // doesn't strip, this fails loud instead of degrading to vector-only fusion.
            let text_store = self.text(token)?;
            let text_fut = async {
                let _phase = khive_storage::request_context::enter_search_phase(
                    khive_storage::request_context::SearchPhase::TextSearch,
                );
                text_store
                    .search(TextSearchRequest {
                        query: query_text.to_string(),
                        mode: text_mode,
                        filter: Some(TextFilter {
                            namespaces: visible_ns.clone(),
                            // Push the entity-kind filter into the FTS query. Without it the
                            // text arm returns the top `candidates` rows across EVERY entity
                            // kind in the namespace and the kind is applied only afterwards,
                            // so when one kind dominates the lexical ranking a search for a
                            // rarer kind gets back fewer rows than exist, or none. The
                            // `EntityFilter.kinds` check below stays as the backstop.
                            record_kinds: entity_kind
                                .map(|kind| vec![kind.to_string()])
                                .unwrap_or_default(),
                            ..TextFilter::default()
                        }),
                        top_k: candidates,
                        snippet_chars: 200,
                    })
                    .await
            };
            let text_fut = crate::stage_seam::text_stage(text_fut);
            // The stages read nothing from each other, so they run together; a text error wins.
            let vector_fut = async {
                let _phase = khive_storage::request_context::enter_search_phase(
                    khive_storage::request_context::SearchPhase::VectorSearch,
                );
                match vector_pool {
                    Some(pool) => Ok((pool, None)),
                    None => {
                        self.hybrid_vector_stage(
                            token,
                            query_text,
                            query_vector,
                            candidates,
                            vector_similarity_floor,
                            tolerate_vector_error,
                        )
                        .await
                    }
                }
            };
            let (text_search_result, vector_result) = tokio::join!(text_fut, vector_fut);
            // FtsPasses is counted inside the store's `search()` (khive-db
            // stores/text.rs), only once a real FTS5 statement is prepared —
            // an empty/fully-sanitized query short-circuits there before any
            // statement exists and must not count.
            let text_hits = crate::error::fts_text_leg_or_err(
                text_search_result.map_err(RuntimeError::from),
                "hybrid_search",
                query_text,
            )?;
            let (vector_hits, vector_error) = vector_result?;

            // Each arm fetched `candidates` independently, so their union can contain
            // twice that many distinct IDs. Keep the complete fetched pool through
            // ranking and the alive/kind/tag/property filters below; reusing one arm's
            // cap here lets stale or filtered hits hide live candidates from the other.
            let fusion_limit = text_hits.len().saturating_add(vector_hits.len());
            let mut fused = {
                let _phase = khive_storage::request_context::enter_search_phase(
                    khive_storage::request_context::SearchPhase::Fusion,
                );
                rrf_fuse(text_hits, vector_hits, fusion_limit, query_text)
            };

            // tags_any has a SQL column and is pushed into query_entities; properties
            // filtering has no SQL column and is applied in Rust below on the fetched records.
            if !fused.is_empty() {
                let candidate_ids: Vec<Uuid> = fused.iter().map(|h| h.entity_id).collect();
                let _phase = khive_storage::request_context::enter_search_phase(
                    khive_storage::request_context::SearchPhase::EntityHydration,
                );
                let alive_page = self
                    .entities(token)?
                    .query_entities(
                        token.namespace().as_str(),
                        EntityFilter {
                            ids: candidate_ids,
                            kinds: entity_kind.map(|k| vec![k.to_string()]).unwrap_or_default(),
                            entity_types: entity_type
                                .map(|t| vec![t.to_string()])
                                .unwrap_or_default(),
                            namespaces: visible_ns,
                            tags_any: tags_any.to_vec(),
                            ..EntityFilter::default()
                        },
                        PageRequest {
                            offset: 0,
                            limit: u32::try_from(fused.len()).unwrap_or(u32::MAX),
                        },
                    )
                    .await?;
                drop(_phase);
                let mut entity_meta: HashMap<Uuid, (String, Option<String>)> = HashMap::new();
                let mut alive: HashSet<Uuid> = HashSet::new();
                for e in alive_page.items {
                    // Drop non-matching candidates here, before the alive set is built,
                    // so they're excluded ahead of truncation.
                    if let Some(pf) = properties_filter {
                        if !properties_match(e.properties.as_ref(), pf) {
                            continue;
                        }
                    }
                    alive.insert(e.id);
                    entity_meta.insert(e.id, (e.name, e.description));
                }

                fused.retain(|h| alive.contains(&h.entity_id));

                // Enrich vector-only hits (title/snippet == None) from entity record.
                for hit in &mut fused {
                    if let Some((name, description)) = entity_meta.get(&hit.entity_id) {
                        if hit.title.is_none() {
                            hit.title = Some(name.clone());
                        }
                        if hit.snippet.is_none() {
                            hit.snippet = description.clone();
                        }
                    }
                }
            }

            fused.truncate(limit as usize);
            Ok((fused, vector_error))
        })
        .await?
    }

    async fn embed_backfill_page(
        &self,
        token: &NamespaceToken,
        model_name: &str,
        inputs: &[(Uuid, String)],
    ) -> Vec<Option<DocumentEmbeddingOutcome>> {
        let texts: Vec<String> = inputs.iter().map(|(_, text)| text.clone()).collect();
        match self
            .embed_document_batch_with_model_outcomes_for_token(token, model_name, &texts)
            .await
        {
            Ok(outcomes) => outcomes.into_iter().map(Some).collect(),
            Err(error) => {
                tracing::warn!(
                    model = %model_name,
                    error = %error,
                    "backfill_missing_embeddings: batch embed failed; retrying records individually"
                );
                let mut outcomes = Vec::with_capacity(inputs.len());
                for (id, text) in inputs {
                    match self
                        .embed_document_with_model_outcome_for_token(token, model_name, text)
                        .await
                    {
                        Ok(outcome) => outcomes.push(Some(outcome)),
                        Err(error) => {
                            tracing::warn!(
                                id = %id,
                                model = %model_name,
                                error = %error,
                                "backfill_missing_embeddings: record embed failed"
                            );
                            outcomes.push(None);
                        }
                    }
                }
                outcomes
            }
        }
    }

    /// Backfill vector and FTS index entries for entities and notes that are missing them.
    ///
    /// Intended to run once at startup as a background task (warm-up sequence steps 2–4).
    /// Queries the SQL substrate for entity bodies and note contents that have no
    /// corresponding entry in an eligible embedding model's vector store, then
    /// embeds and inserts them. FTS entries missing for notes are also repopulated.
    ///
    /// The operation is best-effort: individual embed/insert failures are logged and
    /// skipped rather than aborting the whole backfill. If no embedding models are
    /// registered, returns immediately with 0.
    ///
    /// Returns the total number of records backfilled across all models.
    pub async fn backfill_missing_embeddings(&self, token: &NamespaceToken) -> RuntimeResult<u64> {
        use khive_storage::types::{SqlRow, SqlStatement, SqlValue};

        let model_names = self.registered_embedding_model_names();
        if model_names.is_empty() {
            tracing::debug!(
                "backfill_missing_embeddings: no embedding models registered, skipping"
            );
            return Ok(0);
        }

        let ns = token.namespace().as_str().to_string();
        let mut total_backfilled = 0u64;

        for model_name in &model_names {
            let mut model_truncation = EmbeddingTruncationReport::default();
            match self.vectors_for_model(token, model_name) {
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(model = %model_name, error = %error,
                        "backfill_missing_embeddings: vector store unavailable");
                    continue;
                }
            };
            // Must match vec_model_key's naming logic.
            let vec_table = format!("vec_{}", sanitize_key(model_name));

            // --- Entities: embed the canonical body where no vector exists ---
            // Keyset pagination advances past failed or ineligible records too;
            // they must not keep the first page full forever.
            const PAGE_SIZE: usize = EMBEDDING_BATCH_PAGE_SIZE;
            let mut entity_total = 0usize;
            let mut entity_cursor = String::new();
            loop {
                let entity_sql = SqlStatement {
                    sql: format!(
                        "SELECT id FROM entities \
                         WHERE namespace = ?1 AND deleted_at IS NULL AND id > ?3 \
                         AND id NOT IN (\
                             SELECT subject_id FROM {vec_table} \
                             WHERE namespace = ?1 AND embedding_model = ?2 \
                         ) ORDER BY id LIMIT {PAGE_SIZE}"
                    ),
                    params: vec![
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(model_name.clone()),
                        SqlValue::Text(entity_cursor.clone()),
                    ],
                    label: Some("backfill_entities".into()),
                };

                let entity_rows: Vec<SqlRow> = {
                    let sql = self.sql();
                    let reader_result = sql.reader().await;
                    #[cfg(any(test, feature = "fault-injection"))]
                    let reader_result = if BACKFILL_READER_FAIL.with(|c| c.get()) {
                        BACKFILL_READER_FAIL.with(|c| c.set(false));
                        Err(khive_storage::StorageError::Pool {
                            operation: "reader".into(),
                            message: "injected failure".into(),
                        })
                    } else {
                        reader_result
                    };
                    let mut reader = reader_result.map_err(RuntimeError::Storage)?;
                    reader
                        .query_all(entity_sql)
                        .await
                        .map_err(RuntimeError::Storage)?
                };

                let batch_len = entity_rows.len();
                entity_total += batch_len;
                if batch_len == 0 {
                    break;
                }
                entity_cursor = match entity_rows.last().and_then(|row| row.columns.first()) {
                    Some(column) => match &column.value {
                        SqlValue::Text(id) => id.clone(),
                        _ => {
                            return Err(RuntimeError::Internal(
                                "backfill entity ID is not text".into(),
                            ))
                        }
                    },
                    None => {
                        return Err(RuntimeError::Internal(
                            "backfill entity page is empty".into(),
                        ))
                    }
                };

                let entity_store = self.entities(token)?;
                let mut entities = Vec::with_capacity(batch_len);
                let mut inputs = Vec::with_capacity(batch_len);
                for row in &entity_rows {
                    let Some(SqlValue::Text(id)) = row.columns.first().map(|column| &column.value)
                    else {
                        continue;
                    };
                    let Ok(id) = id.parse::<Uuid>() else { continue };
                    let Some(entity) = entity_store.get_entity(id).await? else {
                        continue;
                    };
                    if entity.namespace != ns || entity.deleted_at.is_some() {
                        continue;
                    }
                    let text = crate::curation::entity_embedding_text(&entity);
                    if text.trim().is_empty() {
                        continue;
                    }
                    inputs.push((id, text));
                    entities.push(entity);
                }
                let outcomes = self.embed_backfill_page(token, model_name, &inputs).await;
                for (entity, outcome) in entities.into_iter().zip(outcomes) {
                    let Some(outcome) = outcome else { continue };
                    model_truncation.observe(&outcome);
                    match self
                        .publish_entity_vector_revision(token, &entity, model_name, &outcome.vector)
                        .await
                    {
                        Ok(true) => total_backfilled += 1,
                        Ok(false) => {}
                        Err(error) => tracing::warn!(
                            id = %entity.id, model = %model_name, error = %error,
                            "backfill_missing_embeddings: entity vector insert failed"
                        ),
                    }
                }

                if batch_len < PAGE_SIZE {
                    break;
                }
            }

            // --- Notes: embed content where no vector entry exists ---
            let text_store = self.text_for_notes(token).ok();
            let note_store = self.notes(token).ok();
            let mut note_total = 0usize;
            // Excluded kinds remain absent from this model's vector table.
            // Advancing by id prevents a full page of them from repeating.
            let mut note_cursor = String::new();
            loop {
                // Only the id is selected here; the full Note is fetched below so
                // note_fts_document gets all fields and stays parity-correct.
                let note_sql = SqlStatement {
                    sql: format!(
                        "SELECT id FROM notes \
                         WHERE namespace = ?1 AND deleted_at IS NULL AND id > ?3 \
                         AND id NOT IN (\
                             SELECT subject_id FROM {vec_table} \
                             WHERE namespace = ?1 AND embedding_model = ?2 \
                         ) ORDER BY id LIMIT {PAGE_SIZE}"
                    ),
                    params: vec![
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(model_name.clone()),
                        SqlValue::Text(note_cursor.clone()),
                    ],
                    label: Some("backfill_notes".into()),
                };

                let note_rows: Vec<SqlRow> = {
                    let sql = self.sql();
                    let reader_result = sql.reader().await;
                    #[cfg(any(test, feature = "fault-injection"))]
                    let reader_result = if BACKFILL_READER_FAIL.with(|c| c.get()) {
                        BACKFILL_READER_FAIL.with(|c| c.set(false));
                        Err(khive_storage::StorageError::Pool {
                            operation: "reader".into(),
                            message: "injected failure".into(),
                        })
                    } else {
                        reader_result
                    };
                    let mut reader = reader_result.map_err(RuntimeError::Storage)?;
                    reader
                        .query_all(note_sql)
                        .await
                        .map_err(RuntimeError::Storage)?
                };

                let batch_len = note_rows.len();
                note_total += batch_len;
                if batch_len == 0 {
                    break;
                }
                note_cursor = match note_rows.last().and_then(|row| row.columns.first()) {
                    Some(column) => match &column.value {
                        SqlValue::Text(id) => id.clone(),
                        _ => {
                            return Err(RuntimeError::Internal(
                                "backfill note ID is not text".into(),
                            ))
                        }
                    },
                    None => {
                        return Err(RuntimeError::Internal("backfill note page is empty".into()))
                    }
                };

                let mut notes_for_batch = Vec::with_capacity(batch_len);
                let mut inputs = Vec::with_capacity(batch_len);
                for row in &note_rows {
                    let id_str = row.columns.first().and_then(|c| {
                        if let SqlValue::Text(s) = &c.value {
                            Some(s.clone())
                        } else {
                            None
                        }
                    });

                    let Some(id_str) = id_str else {
                        continue;
                    };
                    let Ok(id) = id_str.parse::<Uuid>() else {
                        continue;
                    };

                    let note = match &note_store {
                        Some(store) => match store.get_note(id).await {
                            Ok(Some(n)) => n,
                            _ => continue,
                        },
                        None => continue,
                    };

                    if note.content.trim().is_empty() {
                        continue;
                    }

                    // Repopulate FTS entry using the shared constructor (first model only
                    // to avoid N identical overwrites per note).
                    if model_names.first().map(|n| n.as_str()) == Some(model_name.as_str()) {
                        if let Some(ref ts) = text_store {
                            if let Err(e) = ts.upsert_document(note_fts_document(&note)).await {
                                tracing::warn!(id = %id, error = %e,
                                    "backfill_missing_embeddings: note FTS upsert failed");
                            }
                        }
                    }

                    if !self
                        .embedding_models_for_note_kind(&note.kind)
                        .contains(model_name)
                    {
                        continue;
                    }

                    inputs.push((id, note.content.clone()));
                    notes_for_batch.push(note);
                }

                let outcomes = self.embed_backfill_page(token, model_name, &inputs).await;
                for (note, outcome) in notes_for_batch.into_iter().zip(outcomes) {
                    let Some(outcome) = outcome else { continue };
                    model_truncation.observe(&outcome);
                    match self
                        .publish_note_vector_revision(token, &note, model_name, &outcome.vector)
                        .await
                    {
                        Ok(true) => total_backfilled += 1,
                        Ok(false) => {}
                        Err(error) => tracing::warn!(
                            id = %note.id, model = %model_name, error = %error,
                            "backfill_missing_embeddings: note vector insert failed"
                        ),
                    }
                }

                if batch_len < PAGE_SIZE {
                    break;
                }
            }

            tracing::info!(
                model = %model_name,
                namespace = %ns,
                entities = entity_total,
                notes = note_total,
                truncated = model_truncation.truncated,
                discarded_bytes = model_truncation.discarded_bytes,
                "backfill_missing_embeddings: model pass complete"
            );
        }

        tracing::info!(
            namespace = %ns,
            total_backfilled = total_backfilled,
            "backfill_missing_embeddings: finished"
        );

        Ok(total_backfilled)
    }

    /// Sweep orphaned vector entries for all registered embedding models.
    ///
    /// A vector entry is orphaned when its `subject_id` no longer exists as a
    /// live row in the entity, note, or knowledge-atom tables (i.e. either the
    /// row is absent or has `deleted_at IS NOT NULL`). Orphaned entries
    /// accumulate after hard-deletes because the vector store and SQL
    /// substrate are decoupled.
    ///
    /// Iterates over every registered embedding model and calls
    /// [`khive_storage::VectorStore::orphan_sweep`] for the token's namespace. Models whose
    /// backend returns [`khive_storage::StorageError::Unsupported`] are skipped without error —
    /// this preserves forward-compat when a newly registered model does not yet
    /// implement sweep.
    ///
    /// Returns the total number of vector rows deleted across all models.
    pub async fn sweep_orphan_vectors(
        &self,
        token: &NamespaceToken,
        max_delete_per_model: u32,
        dry_run: bool,
    ) -> RuntimeResult<u64> {
        use khive_storage::types::OrphanSweepConfig;
        use khive_storage::StorageError;

        let model_names = self.registered_embedding_model_names();
        if model_names.is_empty() {
            tracing::debug!("sweep_orphan_vectors: no embedding models registered, skipping");
            return Ok(0);
        }

        let ns = token.namespace().as_str().to_string();
        let mut total_deleted = 0u64;

        for model_name in &model_names {
            let store = match self.vectors_for_model(token, model_name) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        model = %model_name,
                        error = %e,
                        "sweep_orphan_vectors: failed to get vector store, skipping model"
                    );
                    continue;
                }
            };

            let caps = store.capabilities();
            if !caps.supports_orphan_sweep {
                tracing::debug!(
                    model = %model_name,
                    "sweep_orphan_vectors: backend does not support orphan sweep, skipping"
                );
                continue;
            }

            let config = OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: vec![ns.clone()],
                substrate_kinds: vec![],
                max_delete: max_delete_per_model,
                dry_run,
            };

            match store.orphan_sweep(&config).await {
                Ok(result) => {
                    tracing::info!(
                        model = %model_name,
                        namespace = %ns,
                        scanned = result.scanned,
                        deleted = result.deleted,
                        would_delete = result.would_delete,
                        dry_run = dry_run,
                        "sweep_orphan_vectors: sweep complete"
                    );
                    total_deleted += result.deleted;
                }
                Err(StorageError::Unsupported { .. }) => {
                    tracing::debug!(
                        model = %model_name,
                        "sweep_orphan_vectors: backend returned Unsupported, skipping"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        model = %model_name,
                        error = %e,
                        "sweep_orphan_vectors: sweep failed, continuing with other models"
                    );
                }
            }
        }

        tracing::info!(
            namespace = %ns,
            total_deleted = total_deleted,
            dry_run = dry_run,
            "sweep_orphan_vectors: finished"
        );

        Ok(total_deleted)
    }
}

/// Returns `true` when `properties` is a superset of all key-value pairs in `filter`.
///
/// Mirrors the semantics of `khive_pack_kg::handlers::common::props_match` so that the
/// storage-leg predicate is identical to the handler-side post-filter.
pub(crate) fn properties_match(
    properties: Option<&serde_json::Value>,
    filter: &serde_json::Value,
) -> bool {
    let required = match filter.as_object() {
        Some(obj) if !obj.is_empty() => obj,
        _ => return true,
    };
    let actual = match properties.and_then(serde_json::Value::as_object) {
        Some(obj) => obj,
        None => return false,
    };
    required
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|av| av == v))
}

/// Score bonus applied when an entity's title is an exact case-insensitive match for
/// the query. Dominates RRF scores (~0.09–0.18 range with k=10) so that an exact
/// name match always ranks above any partial or semantic match.
const EXACT_MATCH_BOOST: f64 = 0.5;

/// Fuse text + vector hits with Reciprocal Rank Fusion (k=10).
///
/// Scoring and the merge of each id's labels come from the shared labelled fusion in
/// `khive-retrieval`. Entity search keeps its own k=10 and exact-match boosting here.
/// Hits in both lists get RRF scores summed. If `query_text` exactly matches
/// (case-insensitive) the title a fused hit carries from the text hits, a bonus of
/// `EXACT_MATCH_BOOST` is added to ensure exact-name matches dominate.
/// Sort by fused score, take top-`limit`.
fn rrf_fuse(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    limit: usize,
    query_text: &str,
) -> Vec<SearchHit> {
    let mut text_arm = Vec::new();
    for (rank, hit) in text_hits.into_iter().enumerate() {
        let label = HitLabel {
            rank,
            signals: SearchSignals {
                vector_similarity: None,
                keyword_score: Some(hit.score),
            },
            source: SearchSource::Text,
            title: hit.title,
            snippet: hit.snippet,
        };
        text_arm.push((hit.subject_id, label));
    }
    let mut vector_arm = Vec::new();
    for (rank, hit) in vector_hits.into_iter().enumerate() {
        let label = HitLabel {
            rank,
            signals: SearchSignals {
                vector_similarity: Some(hit.score),
                keyword_score: None,
            },
            source: SearchSource::Vector,
            title: None,
            snippet: None,
        };
        vector_arm.push((hit.subject_id, label));
    }

    let fused = fuse_labelled(
        vec![text_arm, vector_arm],
        RRF_K,
        combine_leg_first_appearance,
    );

    // The bonus joins the full fused order, so it decides the sort and the cut below.
    let query_lower = query_text.to_lowercase();
    let boost = DeterministicScore::from_f64(EXACT_MATCH_BOOST);
    let mut hits = Vec::with_capacity(fused.len());
    for (entity_id, score, label) in fused {
        let title_lower = label.title.as_deref().map(str::to_lowercase);
        let exact = title_lower.as_deref() == Some(query_lower.as_str());
        let score = if exact { score + boost } else { score };
        hits.push(SearchHit {
            entity_id,
            score,
            rank_score_kind: RankScoreKind::Rrf,
            signals: label.signals,
            source: label.source,
            title: label.title,
            snippet: label.snippet,
        });
    }

    hits.sort_by(|a, b| {
        khive_score::cmp_desc_then_id(a.score, &a.entity_id, b.score, &b.entity_id)
    });
    hits.truncate(limit);
    hits
}

#[cfg(test)]
mod rrf_fuse_label_tests;

#[cfg(test)]
#[path = "retrieval_tests.rs"]
mod tests;
