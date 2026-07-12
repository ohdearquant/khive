//! `khive-pack-workspace`  -  the workspace pack (issue #873 v0).
//!
//! Registers a pack-owned `workspace` entity kind: a durable, name-and-
//! `schema_version`-bearing container with an optional, mutable,
//! non-unique `filesystem_path` locator property. Contributes five additive
//! `contains` endpoint rules so a workspace entity can hold git `issue` /
//! `pull_request` / `commit` notes, GTD `task` notes, and `session` notes as
//! typed graph membership  -  no new `EdgeRelation`, no pack-private table.
//!
//! v0 exposes zero new verbs: create a workspace via `create(kind="workspace",
//! name=..., properties={schema_version: 1, ...})`, resolve or create a
//! member record, then `link(source_id=<workspace>, target_id=<member>,
//! relation="contains")`.

mod hook;
mod pack;
mod vocab;

pub use pack::WorkspacePack;
