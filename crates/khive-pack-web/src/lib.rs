//! Web pack (ADR-191): `site`/`page`/`resource` ontology and the
//! `fetch`/`extract`/`ingest`/`search`/`refresh` verbs over HTTP(S) egress
//! policy, the runtime's blob store, and its create/update/link seam.
//! Supersedes ADR-175's local-manifest-only pack.

mod confinement;
mod egress;
mod entities;
mod extract;
mod fetch;
#[cfg(test)]
mod fhcrc_probe_tests;
pub mod identity;
mod ingest;
mod namespace;
mod pack;
pub mod producer;
mod receipt;
mod refresh;
mod search;
mod vocab;

pub use pack::WebPack;
