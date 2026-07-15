//! KG pack verb handlers — split into one file per verb group.

mod common;
mod context;
mod create;
mod get;
mod graph;
mod link;
mod list;
mod merge;
mod params;
mod proposal;
mod resolve;
mod search;
mod stats;
mod update;

pub(crate) use common::{canonical_entity_kind, canonical_note_kind, parse_relation};

/// ADR-099 B3: real `pub` re-export so kkernel's `--atomic` seam validates through the
/// SAME canonical param structs the handlers deserialize, reproducing
/// `#[serde(deny_unknown_fields)]` rejection with no duplicated key list. See
/// `docs/api/entity-kind-validation.md` for the full ADR-099 B3 rationale.
pub use params::{DeleteParams, LinkParams, UpdateParams};

/// ADR-099 B3 (findings 1, 3, 4): real `pub` re-export so kkernel's `--atomic` seam
/// resolves kinds/ids and renders results through the exact canonical logic the
/// handlers use, rather than reimplementing it.
pub use common::{
    normalize_entity_timestamps, resolve_kind_spec, resolve_uuid_unfiltered,
    resolve_uuid_unfiltered_including_deleted, KindSpec,
};

#[cfg(test)]
pub(crate) use common::{
    ensure_note_kind, normalize_entity_timestamps_array, tags_match_any,
    valid_relations_for_entity_pair, validate_weight, walk_timestamps, ListParams, ProposeParams,
    ReviewParams, SearchParams, WithdrawParams,
};

#[cfg(test)]
mod tests;
