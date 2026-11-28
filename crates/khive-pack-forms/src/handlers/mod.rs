//! The forms verbs.

mod check;
mod lines;
mod params;
mod read;
mod resolve;
mod views;
mod write;

use khive_runtime::{KhiveRuntime, NamespaceToken, VerbRegistry};

pub(crate) use check::check;
pub(crate) use read::{export, lint, neighbourhood};
pub(crate) use resolve::resolve;
pub(crate) use write::{add_item, load, replay, same, split};

/// What every forms verb runs against.
pub(crate) struct Ctx<'a> {
    pub(crate) runtime: &'a KhiveRuntime,
    pub(crate) registry: &'a VerbRegistry,
    pub(crate) token: &'a NamespaceToken,
}
