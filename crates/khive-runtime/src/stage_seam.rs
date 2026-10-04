//! Test seam for the text stage of entity search and note search.
//!
//! Both searches run their text stage and their vector stage together. A test that runs a
//! search inside `TEXT_STAGE_DOUBLE.scope(..)` replaces the text stage with a future of its
//! own, so it can hold that stage at a barrier shared with the vector stage, or make it fail.
//! Outside such a scope, and in every non-test build, the stage runs as written.

use std::future::Future;
#[cfg(test)]
use std::pin::Pin;
#[cfg(test)]
use std::sync::Arc;

use khive_storage::types::TextSearchHit;
use khive_storage::StorageError;

pub(crate) type TextStageResult = Result<Vec<TextSearchHit>, StorageError>;

#[cfg(test)]
pub(crate) type BoxedTextStage = Pin<Box<dyn Future<Output = TextStageResult> + Send>>;

/// Builds the future that takes the place of the text stage; called once per stage run.
#[cfg(test)]
pub(crate) type TextStageDouble = Arc<dyn Fn() -> BoxedTextStage + Send + Sync>;

#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEXT_STAGE_DOUBLE: TextStageDouble;
}

/// Awaits `stage`, or the scoped test double in its place.
pub(crate) async fn text_stage(stage: impl Future<Output = TextStageResult>) -> TextStageResult {
    #[cfg(test)]
    {
        if let Ok(double) = TEXT_STAGE_DOUBLE.try_with(Arc::clone) {
            return double().await;
        }
    }
    stage.await
}
