use std::{hash::Hash, num::NonZeroUsize};

use async_trait::async_trait;
use khive_score::DeterministicScore;
use khive_storage::{StorageError, TextQueryMode};

/// An identity, deterministic score, and caller-owned label.
pub type Scored<I, L> = (I, DeterministicScore, L);

/// Already-authorized selectors; these do not replace the adapter's authorization.
pub struct SearchScope<'a, F: ?Sized> {
    /// Exact namespace selection. An empty selection never means all namespaces.
    pub namespaces: &'a [&'a str],
    /// Caller-specific record filters.
    pub filter: &'a F,
}

/// The vector adapter's existing query representation.
pub enum VectorInput<'a> {
    /// Let the adapter embed this text.
    Text(&'a str),
    /// Use an already prepared embedding.
    Embedding(&'a [f32]),
}

/// Inputs for one independently scheduled retrieval arm.
pub enum ArmInput<'a> {
    /// One physical vector binding and one model.
    Vector {
        /// Engine identifier understood by the adapter.
        engine: &'a str,
        /// Model identifier understood by the adapter.
        model_id: &'a str,
        /// Existing physical binding, when the adapter exposes one.
        space_id: Option<&'a str>,
        /// Query text or embedding.
        input: VectorInput<'a>,
    },
    /// One lexical search.
    Text {
        /// Query text.
        query: &'a str,
        /// Existing storage query mode.
        mode: &'a TextQueryMode,
        /// Requested snippet window.
        snippet_chars: usize,
    },
}

/// Borrowed request passed unchanged to an arm, except prepared vector input.
pub struct ArmRequest<'a, F: ?Sized> {
    /// This arm's authorized selection.
    pub scope: SearchScope<'a, F>,
    /// The selected input.
    pub input: &'a ArmInput<'a>,
    /// Maximum returned candidates, including zero.
    pub candidate_limit: u32,
}

/// Which retrieval route completed, independently of its evidence or hit count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArmRoute {
    /// The selected primary route.
    Primary,
    /// An adapter-approved exact fallback.
    ExactFallback,
}

/// A completed arm, including successful empty results and degradation evidence.
pub struct ArmCompletion<I, L, D> {
    /// Candidates in the adapter's original order.
    pub hits: Vec<Scored<I, L>>,
    /// Completed route.
    pub route: ArmRoute,
    /// Caller-owned evidence, retained even for empty primary completions.
    pub evidence: D,
}

/// Typed failure classification made before legacy error conversion.
pub enum ArmFailure<E, D> {
    /// A stage-local failure that the plan may tolerate.
    Ordinary(E),
    /// A failure that no arm policy may tolerate.
    Fatal(E),
    /// The selected index or capability is unavailable.
    Unavailable(D),
}

/// Whether ordinary failure or unavailability yields an empty arm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailurePolicy {
    /// Refuse ordinary failure and unavailability.
    Required,
    /// Retain typed failure or unavailability evidence with an empty stream.
    TolerateOrdinary,
}

/// One borrowed retrieval provider. Owned payloads need neither cloning nor formatting.
#[async_trait]
pub trait ExecutorArm: Send + Sync {
    /// Stable candidate identity.
    type Id: Eq + Hash + Clone + Send + Sync;
    /// Caller-owned hit metadata.
    type Label: Send;
    /// Caller-owned completion or unavailability evidence.
    type Evidence: Send;
    /// Caller-specific filter, which may be unsized.
    type Filter: Sync + ?Sized;
    /// Original caller error.
    type Error: Send;

    /// Search one selected arm under the inherited request context.
    async fn search<'a>(
        &'a self,
        request: ArmRequest<'a, Self::Filter>,
    ) -> Result<
        ArmCompletion<Self::Id, Self::Label, Self::Evidence>,
        ArmFailure<Self::Error, Self::Evidence>,
    >;
}

/// One stable slot in an executor plan.
pub struct ArmSpec<'a, A: ExecutorArm + ?Sized> {
    /// Unique nonempty name used in the returned report.
    pub key: &'a str,
    /// Borrowed provider; trait objects are supported.
    pub adapter: &'a A,
    /// This arm's selection, independent of the combined hydration selection.
    pub scope: SearchScope<'a, A::Filter>,
    /// `None` explicitly skips this slot.
    pub input: Option<ArmInput<'a>>,
    /// Maximum raw candidates from this slot, including zero.
    pub candidate_limit: u32,
    /// Stage-local failure treatment.
    pub failure: FailurePolicy,
}

/// One preparation result in the same order as the preparer's input requests.
pub struct PreparedVector<D> {
    /// `None` skips this model's search while retaining its evidence.
    pub embedding: Option<Vec<f32>>,
    /// Per-model preparation evidence.
    pub evidence: D,
}

/// A completed embedding barrier. No vector searches run before it completes.
pub struct VectorPreparation<D> {
    /// Exactly one result for each supplied request.
    pub arms: Vec<PreparedVector<D>>,
    /// Aggregate preparation evidence, including session-consistency evidence.
    pub evidence: D,
}

/// Optional batched embedding preparation; vector search remains executor-owned.
#[async_trait]
pub trait VectorGroupPreparer: Send + Sync {
    /// Caller-specific filters.
    type Filter: Sync + ?Sized;
    /// Owned per-model and aggregate evidence.
    type Evidence: Send;
    /// Original preparation error.
    type Error: Send;

    /// Prepare selected requests paired with positions in the original vector-slot list.
    /// `vector_slot_count` includes skipped slots; returned results match request order.
    async fn prepare<'a>(
        &'a self,
        requests: Vec<(usize, ArmRequest<'a, Self::Filter>)>,
        vector_slot_count: usize,
    ) -> Result<VectorPreparation<Self::Evidence>, Self::Error>;
}

/// Final state of an arm before candidate preparation or fusion.
pub enum ArmState<D, E> {
    /// Explicitly skipped, empty selection, or preparation withheld an embedding.
    Skipped,
    /// Search completed, even if no candidates were returned.
    Completed {
        /// Completed route.
        route: ArmRoute,
        /// Original completion evidence.
        evidence: D,
    },
    /// Tolerated unavailability.
    Unavailable(D),
    /// Tolerated ordinary failure.
    Failed(E),
}

/// Raw retrieval accounting, unaffected by later filtering or fusion.
pub struct ArmReport<D, E> {
    /// Original slot name.
    pub key: String,
    /// Requested raw candidate limit.
    pub requested: u32,
    /// Raw successful candidate count.
    pub returned: usize,
    /// Optional evidence from the embedding barrier.
    pub preparation: Option<D>,
    /// Search completion or failure classification.
    pub state: ArmState<D, E>,
}

/// One fully prepared output item before the executor's final cut.
pub struct HydratedHit<I, L, T> {
    /// Retained candidate identity.
    pub id: I,
    /// Final caller-ranked score.
    pub score: DeterministicScore,
    /// Caller-owned label.
    pub label: L,
    /// Hydrated record or other caller result.
    pub item: T,
}

/// Ordered arm streams and their typed completion evidence.
pub struct CollectedArms<I, L, D, E> {
    /// One stream per original plan slot, including skipped slots.
    pub arms: Vec<Vec<Scored<I, L>>>,
    /// One raw report per original slot.
    pub reports: Vec<ArmReport<D, E>>,
    /// Optional aggregate embedding-preparation evidence.
    pub vector_preparation: Option<D>,
}

/// Batched pre-fusion filtering plus opaque state reused during hydration.
pub struct PreparedArms<I, L, S> {
    /// Same slots, each retaining an ordered subsequence of its input identities.
    pub arms: Vec<Vec<Scored<I, L>>>,
    /// Caller-owned hydration snapshot or other retained state.
    pub state: S,
}

/// Caller policy and hydration surrounding executor-owned scheduling and fusion.
#[async_trait]
pub trait CandidateHydrator: Send + Sync {
    /// Candidate identity.
    type Id: Eq + Hash + Clone + Send + Sync;
    /// Owned hit metadata.
    type Label: Send;
    /// Owned retrieval evidence.
    type Evidence: Send;
    /// Caller-specific combined filter.
    type Filter: Sync + ?Sized;
    /// Final hydrated item.
    type Item: Send;
    /// Batched state transferred from preparation to hydration.
    type State: Send;
    /// Original caller error.
    type Error: Send;

    /// Apply aggregate outcome policy without changing streams or raw reports.
    fn validate_outcomes(
        &self,
        collected: CollectedArms<Self::Id, Self::Label, Self::Evidence, Self::Error>,
    ) -> Result<CollectedArms<Self::Id, Self::Label, Self::Evidence, Self::Error>, Self::Error>;

    /// Filter all arm streams in one batch before fusion assigns ranks.
    async fn prepare_arms<'a>(
        &'a self,
        scope: SearchScope<'a, Self::Filter>,
        arms: Vec<Vec<Scored<Self::Id, Self::Label>>>,
    ) -> Result<PreparedArms<Self::Id, Self::Label, Self::State>, Self::Error>;

    /// Hydrate, filter and rank the full fused stream before its final limit.
    async fn hydrate_filter_rank<'a>(
        &'a self,
        scope: SearchScope<'a, Self::Filter>,
        state: Self::State,
        candidates: Vec<Scored<Self::Id, Self::Label>>,
    ) -> Result<Vec<HydratedHit<Self::Id, Self::Label, Self::Item>>, Self::Error>;
}

/// Borrowed execution plan. Scheduling precedence is selected by the entry point.
pub struct ExecutorRequest<'a, A: ExecutorArm + ?Sized> {
    /// Diagnostic operation attached to request-stop errors.
    pub operation: &'static str,
    /// Combined selection for batched preparation and hydration.
    pub scope: SearchScope<'a, A::Filter>,
    /// Original stable arm slots.
    pub arms: &'a [ArmSpec<'a, A>],
    /// Maximum concurrently polled arm searches across text and vector groups.
    pub max_in_flight: NonZeroUsize,
    /// Vector slots in increasing original plan order; every vector input belongs here.
    pub vector_slots: &'a [usize],
    /// Optional embedding barrier. `None` preserves each arm's existing input.
    pub vector_preparer: Option<
        &'a dyn VectorGroupPreparer<Filter = A::Filter, Evidence = A::Evidence, Error = A::Error>,
    >,
    /// Checked upper bound on the sum of selected raw candidate limits.
    pub max_candidates: usize,
    /// Final output limit. Zero still executes all selected phases.
    pub final_limit: usize,
}

/// Bounded final results plus raw evidence in original plan order.
pub struct ExecutorOutcome<I, L, D, T, E> {
    /// Hydrated and ranked results after the final cut.
    pub hits: Vec<HydratedHit<I, L, T>>,
    /// Raw arm accounting.
    pub arms: Vec<ArmReport<D, E>>,
    /// Optional aggregate embedding evidence.
    pub vector_preparation: Option<D>,
}

/// Typed executor errors; caller errors are never stringified.
pub enum ExecutorError<E, D> {
    /// Invalid plan shape or bound.
    Invalid(&'static str),
    /// Cancellation or the inherited absolute deadline stopped the request.
    RequestStopped(StorageError),
    /// An intolerable arm error at its original slot.
    Arm {
        /// Original slot.
        slot: usize,
        /// Original error.
        error: E,
    },
    /// Required arm unavailable.
    Unavailable {
        /// Original slot.
        slot: usize,
        /// Original evidence.
        evidence: D,
    },
    /// Batched embedding preparation failed.
    VectorPreparation(E),
    /// Caller aggregate outcome policy refused collected evidence.
    OutcomePolicy(E),
    /// Batched pre-fusion preparation failed.
    Preparation(E),
    /// Fusion strategy resolution failed before any provider work.
    FusionResolution(E),
    /// The resolved fusion operation failed.
    Fusion(E),
    /// Post-fusion hydration or ranking failed.
    Hydration(E),
    /// A provider or callback violated a bound or identity contract.
    Protocol {
        /// Original arm slot, when the violation is arm-specific.
        slot: Option<usize>,
        /// Stable protocol diagnostic.
        message: &'static str,
    },
}
