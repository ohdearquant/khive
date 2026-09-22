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
    pub(crate) fn add_to_result(
        self,
        result: &mut Value,
        items: &[Event],
        default_actor: Option<&str>,
        truncated: bool,
    ) {
        let mut counts: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        for event in items {
            let actor = default_actor.unwrap_or(event.actor.as_str());
            *counts
                .entry(event.verb.clone())
                .or_default()
                .entry(actor.to_owned())
                .or_default() += 1;
        }
        result[BrainPack::truncatable_total_key("counts_by_verb_and_actor", truncated)] =
            json!(counts);
    }
}
