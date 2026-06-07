//! Brain primitives — Beta posteriors, section types, profile state, weight derivation.

pub mod brain_state;
pub mod posterior;
pub mod profile;
pub mod section_state;
pub mod section_type;
pub mod signal;

pub use brain_state::{validate_brain_state_snapshot, BrainState, BrainStateSnapshot};
pub use posterior::{BetaPosterior, EntityPosteriors};
pub use profile::{
    BalancedRecallSnapshot, BalancedRecallState, ProfileBinding, ProfileLifecycle, ProfileRecord,
};
pub use section_state::{
    derive_deterministic_weights, derive_weights, SectionPosteriorSnapshot, SectionPosteriorState,
    DEFAULT_ESS_CAP, DEFAULT_EXPLORATION_EPOCH, DEFAULT_SECTION_WEIGHT_FLOOR, DEFAULT_TAU_0,
    DEFAULT_TAU_EXPLOIT,
};
pub use section_type::SectionType;
pub use signal::{FeedbackEventKind, FeedbackSignal};
