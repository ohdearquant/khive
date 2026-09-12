//! Web origin vocabulary and local manifest ingest (ADR-175).

mod db_target;
mod extract;
mod handlers;
mod manifest;
mod pack;
mod persistence;
mod views;
mod vocab;

pub use extract::WEB_INGEST_NAMESPACE;
pub use pack::WebPack;
