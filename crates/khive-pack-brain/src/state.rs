//! Brain domain types — re-exported from khive-brain-core.

pub use khive_brain_core::posterior::{BetaPosterior, EntityPosteriors};
pub use khive_brain_core::profile::{
    BalancedRecallSnapshot, BalancedRecallState, ProfileBinding, ProfileLifecycle, ProfileRecord,
};
pub use khive_brain_core::section_state::{
    SectionPosteriorSnapshot, SectionPosteriorState, DEFAULT_ESS_CAP,
    DEFAULT_EXPLORATION_EPOCH, DEFAULT_SECTION_WEIGHT_FLOOR, DEFAULT_TAU_0, DEFAULT_TAU_EXPLOIT,
};
pub use khive_brain_core::section_type::SectionType;
pub use khive_brain_core::{
    validate_brain_state_snapshot, BrainState, BrainStateSnapshot,
};
