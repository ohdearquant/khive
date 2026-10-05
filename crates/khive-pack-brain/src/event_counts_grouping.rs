use std::collections::BTreeMap;

use khive_storage::Event;
use serde::Deserialize;
use serde_json::{json, Value};

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
