//! Workspace entity vocabulary, creation validation, and typed membership rules.

mod config;
mod hook;
mod pack;
mod vocab;

pub use config::{WorkspaceMirrorConfig, WorkspaceMirrorConfigError};
pub use pack::WorkspacePack;
