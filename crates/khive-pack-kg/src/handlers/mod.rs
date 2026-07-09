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

/// ADR-099 B3: re-exported as a real `pub` path (not `pub(crate)`) so
/// `kkernel`'s `--atomic` validation seam can reach the SAME canonical
/// param structs `handle_update`/`handle_delete`/`handle_link` deserialize
/// through, reproducing their `#[serde(deny_unknown_fields)]` rejection
/// without a duplicated per-verb key list. `kkernel` already depends on
/// this crate directly (no crate-graph inversion); see
/// `kkernel::atomic_apply::validate_atomic_args`.
pub use params::{DeleteParams, LinkParams, UpdateParams};

/// ADR-099 B3 fix round 5 (findings 1, 3, 4): re-exported as real `pub`
/// paths so `kkernel`'s `--atomic` seam can resolve kinds/ids and render
/// result payloads through the exact canonical logic `handle_update`/
/// `handle_delete`/`handle_link` use, rather than reimplementing it.
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
