//! khive-pack-session — session storage pack for the khive runtime.
//!
//! Registers the `session` note kind and four verbs:
//! `session.store`, `session.list`, `session.resume`, `session.export`.

pub mod handlers;
mod pack;
pub mod vocab;

use khive_runtime::KhiveRuntime;
use khive_types::{HandlerDef, Pack};

pub(crate) use vocab::SESSION_HANDLERS;

/// Session pack — registers the `session` note kind and session lifecycle verbs.
pub struct SessionPack {
    runtime: KhiveRuntime,
}

impl Pack for SessionPack {
    const NAME: &'static str = "session";
    const NOTE_KINDS: &'static [&'static str] = &["session"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &SESSION_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl SessionPack {
    /// Create a new `SessionPack` bound to the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}
