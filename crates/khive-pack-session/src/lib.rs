//! khive-pack-session — session storage pack for the khive runtime.
//!
//! Registers the `session` note kind and three internal subhandlers:
//! `session.store`, `session.list`, `session.get`
//! (operator-only this milestone — see `vocab::SESSION_HANDLERS`).
//! Serialization (`handlers::export::handle_export`) is an in-process helper,
//! not a dispatchable verb.
//!
//! The pack's active feature is a background mirror service (`warm` hook) that
//! live-tails Claude Code session JSONL transcripts into the pack's auxiliary
//! SQL tables.

pub mod handlers;
pub mod mirror;
mod pack;
pub mod vocab;

use khive_runtime::KhiveRuntime;
use khive_types::{HandlerDef, Pack, PackSchemaPlan};

pub(crate) use vocab::{SESSION_HANDLERS, SESSION_SCHEMA_PLAN_STMTS};

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
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: "session",
        statements: &SESSION_SCHEMA_PLAN_STMTS,
    });
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
