//! KG pack verb handlers — split into one file per verb group.

mod common;
mod create;
mod get;
mod graph;
mod link;
mod list;
mod merge;
mod proposal;
mod search;
mod stats;
mod update;

pub(crate) use common::canonical_note_kind;

#[cfg(test)]
pub(crate) use common::{
    ensure_note_kind, normalize_entity_timestamps, normalize_entity_timestamps_array,
    parse_relation, resolve_kind_spec, tags_match_any, valid_relations_for_entity_pair,
    validate_weight, walk_timestamps, KindSpec, ListParams, ProposeParams, ReviewParams,
    SearchParams, UpdateParams, WithdrawParams,
};

#[cfg(test)]
mod tests;
