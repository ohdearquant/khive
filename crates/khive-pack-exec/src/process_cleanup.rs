//! Receipt evidence is independent of direct-child exit and artifact collection.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CleanupScope {
    InitialGroup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CleanupObservation {
    NotAttempted,
    Complete,
    Incomplete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CleanupCertification {
    Unverified,
    CertifiedNone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TreeQuiescence {
    Unverified,
    Certified,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessCleanup {
    pub(crate) scope: CleanupScope,
    pub(crate) observation: CleanupObservation,
    pub(crate) seen_alive: bool,
    pub(crate) certification: CleanupCertification,
    pub(crate) detail: String,
}

impl ProcessCleanup {
    pub(crate) fn seatbelt_unobserved() -> Self {
        // Group signalling and a direct-child wait do not observe escaped descendants.
        Self {
            scope: CleanupScope::InitialGroup,
            observation: CleanupObservation::NotAttempted,
            seen_alive: false,
            certification: CleanupCertification::Unverified,
            detail: "Detached descendant termination is not certified on this backend.".into(),
        }
    }
}
