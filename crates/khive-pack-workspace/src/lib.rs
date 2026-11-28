//! Workspace entity vocabulary, creation validation, and typed membership rules.

mod config;
pub mod cursor;
mod hook;
pub mod mirror;
mod pack;
mod vocab;

pub use config::{WorkspaceMirrorConfig, WorkspaceMirrorConfigError};
pub use pack::WorkspacePack;
