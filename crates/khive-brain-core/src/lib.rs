//! Brain primitives — Beta posteriors, section types, profile state, weight derivation.

pub mod brain_signal;
pub mod brain_state;
pub mod posterior;
pub mod profile;
pub mod query_class;
pub mod section_state;
pub mod section_type;
pub mod signal;
pub mod tunable;

pub use brain_signal::{entity_signal, is_recall_positive, BrainSignal};
pub use brain_state::{
    validate_brain_state_snapshot, validate_brain_state_snapshot_with_capacity, BrainState,
    BrainStateSnapshot,
};
pub use posterior::{BetaPosterior, EntityPosteriors};
pub use profile::{
    resolve_consumer_profile, BalancedRecallSnapshot, BalancedRecallState, ConsumerKind,
    ProfileBinding, ProfileLifecycle, ProfileRecord,
};
pub use query_class::compute_query_class;
pub use section_state::{
    derive_deterministic_weights, derive_weights, SectionPosteriorSnapshot, SectionPosteriorState,
    DEFAULT_ESS_CAP, DEFAULT_EXPLORATION_EPOCH, DEFAULT_SECTION_WEIGHT_FLOOR, DEFAULT_TAU_0,
    DEFAULT_TAU_EXPLOIT,
};
pub use section_type::SectionType;
pub use signal::{FeedbackEventKind, FeedbackSignal};
pub use tunable::{PackTunable, ParameterDef, ParameterSpace};
