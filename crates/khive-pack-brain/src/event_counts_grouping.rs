use std::collections::BTreeMap;

use khive_storage::Event;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::handlers::{event_cost_unit, event_work_class};
use crate::BrainPack;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventCountDimension {
    Verb,
    Actor,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(try_from = "[EventCountDimension; 2]")]
pub(crate) enum EventCountGroupBy {
    VerbActor,
}

impl TryFrom<[EventCountDimension; 2]> for EventCountGroupBy {
    type Error = &'static str;

    fn try_from(dimensions: [EventCountDimension; 2]) -> Result<Self, Self::Error> {
        match dimensions {
            [EventCountDimension::Verb, EventCountDimension::Actor] => Ok(Self::VerbActor),
            _ => Err("group_by supports only [\"verb\", \"actor\"] in that order"),
        }
    }
}

impl EventCountGroupBy {
    #[cfg(test)]
    pub(crate) fn add_to_result(
        self,
        result: &mut Value,
        items: &[Event],
        default_actor: Option<&str>,
        truncated: bool,
    ) {
        let mut cross = EventCountCross::default();
        for event in items {
            cross.observe(event, default_actor);
        }
        cross.add_to_result(result, truncated);
    }
}

#[derive(Default)]
pub(crate) struct EventCountCross {
    counts: BTreeMap<String, BTreeMap<String, u64>>,
}

impl EventCountCross {
    pub(crate) fn observe(&mut self, event: &Event, default_actor: Option<&str>) {
        let actor = default_actor.unwrap_or(event.actor.as_str());
        *self
            .counts
            .entry(event.verb.clone())
            .or_default()
            .entry(actor.to_owned())
            .or_default() += 1;
    }

    pub(crate) fn add_to_result(self, result: &mut Value, truncated: bool) {
        result[BrainPack::truncatable_total_key("counts_by_verb_and_actor", truncated)] =
            json!(self.counts);
    }
}

/// Counts own only output keys and scalar aggregates, never event payloads.
#[derive(Default)]
pub(crate) struct EventCountsAccumulator {
    default_actor: Option<String>,
    group_counts: Option<crate::event_counts_grouping::EventCountCross>,
    counts_by_kind: std::collections::BTreeMap<String, u64>,
    counts_by_actor: std::collections::BTreeMap<String, u64>,
    counts_by_verb: std::collections::BTreeMap<String, u64>,
    by_profile: std::collections::BTreeMap<String, u64>,
    feedback_by_originating_verb: std::collections::BTreeMap<String, u64>,
    // #34: flat and profile-crossed signal counts read the same payload
    // attribution fields stamped by brain.feedback/brain.auto_feedback.
    counts_by_signal: std::collections::BTreeMap<String, u64>,
    by_profile_and_signal:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, u64>>,
    counts_by_work_class: std::collections::BTreeMap<String, u64>,
    total_cost_unit: i64,
    cost_unit_by_verb: std::collections::BTreeMap<String, i64>,
}

impl EventCountsAccumulator {
    pub(crate) fn new(
        default_actor: Option<&str>,
        group_by: Option<crate::event_counts_grouping::EventCountGroupBy>,
    ) -> Self {
        Self {
            default_actor: default_actor.map(str::to_owned),
            group_counts: group_by.map(|_| Default::default()),
            ..Default::default()
        }
    }

    pub(crate) fn observe(&mut self, event: &Event) {
        *self
            .counts_by_kind
            .entry(event.kind.name().to_string())
            .or_insert(0) += 1;
        let actor_key = self.default_actor.as_ref().unwrap_or(&event.actor);
        *self.counts_by_actor.entry(actor_key.clone()).or_insert(0) += 1;
        *self.counts_by_verb.entry(event.verb.clone()).or_insert(0) += 1;
        if event.kind == khive_types::EventKind::FeedbackExplicit {
            let originating_verb = event
                .payload
                .get("originating_verb")
                .and_then(Value::as_str)
                .unwrap_or(event.verb.as_str())
                .to_string();
            *self
                .feedback_by_originating_verb
                .entry(originating_verb)
                .or_insert(0) += 1;
            let profile = event
                .payload
                .get("served_by_profile_id")
                .and_then(Value::as_str)
                .unwrap_or("unspecified")
                .to_string();
            *self.by_profile.entry(profile.clone()).or_insert(0) += 1;
            let signal = event
                .payload
                .get("signal")
                .and_then(Value::as_str)
                .unwrap_or("unspecified")
                .to_string();
            *self.counts_by_signal.entry(signal.clone()).or_insert(0) += 1;
            *self
                .by_profile_and_signal
                .entry(profile)
                .or_default()
                .entry(signal)
                .or_insert(0) += 1;
        }
        if let Some(work_class) = event_work_class(&event.payload) {
            *self
                .counts_by_work_class
                .entry(work_class.to_string())
                .or_insert(0) += 1;
        }
        if let Some(cost_unit) = event_cost_unit(&event.payload) {
            self.total_cost_unit = self.total_cost_unit.saturating_add(cost_unit);
            let entry = self
                .cost_unit_by_verb
                .entry(event.verb.clone())
                .or_insert(0);
            *entry = entry.saturating_add(cost_unit);
        }
        if let Some(cross) = &mut self.group_counts {
            cross.observe(event, self.default_actor.as_deref());
        }
    }

    pub(crate) fn add_to_result(self, result: &mut Value, truncated: bool) {
        result["counts_by_kind"] = json!(self.counts_by_kind);
        result["counts_by_actor"] = json!(self.counts_by_actor);
        result["counts_by_verb"] = json!(self.counts_by_verb);
        if let Some(cross) = self.group_counts {
            cross.add_to_result(result, truncated);
        }
        if !self.by_profile.is_empty() {
            result["by_profile"] = json!(self.by_profile);
        }
        if !self.feedback_by_originating_verb.is_empty() {
            result["feedback_by_originating_verb"] = json!(self.feedback_by_originating_verb);
        }
        if !self.counts_by_signal.is_empty() {
            result["counts_by_signal"] = json!(self.counts_by_signal);
        }
        if !self.by_profile_and_signal.is_empty() {
            result["by_profile_and_signal"] = json!(self.by_profile_and_signal);
        }
        if !self.counts_by_work_class.is_empty() {
            result["counts_by_work_class"] = json!(self.counts_by_work_class);
        }
        if !self.cost_unit_by_verb.is_empty() {
            result[BrainPack::truncatable_total_key("total_cost_unit", truncated)] =
                json!(self.total_cost_unit);
            result["cost_unit_by_verb"] = json!(self.cost_unit_by_verb);
        }
    }
}
