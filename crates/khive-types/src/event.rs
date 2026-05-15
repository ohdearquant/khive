//! Event substrate — universal system log.
//!
//! Every verb execution produces an Event. Audit, usage metering, derived
//! state, and evolutionary learning (edge reinforcement, traversal history)
//! are all computed via Fold over the Event stream.

extern crate alloc;
use alloc::string::String;
use core::fmt;

use crate::{Header, Id128, SubstrateKind};

/// A system event. Append-only, never mutated or deleted.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Event {
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub header: Header,
    /// The verb that was executed (e.g., "create", "search", "traverse").
    pub verb: String,
    /// Which substrate type was acted upon.
    pub substrate: SubstrateKind,
    /// Who performed the action (free-form actor string).
    pub actor: String,
    /// Outcome of the verb execution.
    pub outcome: EventOutcome,
    /// Optional verb-specific structured data (JSON in DB).
    pub data: Option<String>,
    /// Duration of the verb execution in microseconds.
    pub duration_us: u64,
    /// ID of the substrate record that was acted upon, if applicable.
    pub target_id: Option<Id128>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EventOutcome {
    #[default]
    Success,
    Denied,
    Error,
}

impl EventOutcome {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Denied => "denied",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for EventOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Builder for events. Used by the verb dispatch path.
pub struct EventBuilder {
    verb: String,
    substrate: SubstrateKind,
    actor: String,
    outcome: EventOutcome,
    data: Option<String>,
    duration_us: u64,
    target_id: Option<Id128>,
}

impl EventBuilder {
    pub fn new(
        verb: impl Into<String>,
        substrate: SubstrateKind,
        actor: impl Into<String>,
    ) -> Self {
        Self {
            verb: verb.into(),
            substrate,
            actor: actor.into(),
            outcome: EventOutcome::Success,
            data: None,
            duration_us: 0,
            target_id: None,
        }
    }

    pub fn outcome(mut self, outcome: EventOutcome) -> Self {
        self.outcome = outcome;
        self
    }

    pub fn data(mut self, data: impl Into<String>) -> Self {
        self.data = Some(data.into());
        self
    }

    pub fn duration_us(mut self, us: u64) -> Self {
        self.duration_us = us;
        self
    }

    pub fn target_id(mut self, id: Id128) -> Self {
        self.target_id = Some(id);
        self
    }

    pub fn build(self, header: Header) -> Event {
        Event {
            header,
            verb: self.verb,
            substrate: self.substrate,
            actor: self.actor,
            outcome: self.outcome,
            data: self.data,
            duration_us: self.duration_us,
            target_id: self.target_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Namespace, Timestamp};

    fn header() -> Header {
        Header::new(
            Id128::from_u128(1),
            Namespace::default(),
            Timestamp::from_secs(1700000000),
        )
    }

    #[test]
    fn event_builder() {
        let event = EventBuilder::new("search", SubstrateKind::Note, "agent:research")
            .outcome(EventOutcome::Success)
            .duration_us(1500)
            .target_id(Id128::from_u128(42))
            .build(header());

        assert_eq!(event.verb, "search");
        assert_eq!(event.substrate, SubstrateKind::Note);
        assert_eq!(event.actor, "agent:research");
        assert_eq!(event.outcome, EventOutcome::Success);
        assert_eq!(event.duration_us, 1500);
        assert_eq!(event.target_id, Some(Id128::from_u128(42)));
    }

    #[test]
    fn denied_outcome() {
        let event = EventBuilder::new("create", SubstrateKind::Note, "user:ocean")
            .outcome(EventOutcome::Denied)
            .build(header());
        assert_eq!(event.outcome, EventOutcome::Denied);
    }
}
