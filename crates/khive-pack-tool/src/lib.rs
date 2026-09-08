//! khive-pack-tool: a registry of callable things (tools, skills, plugins,
//! khive verbs) with ontological discovery and tool-use policy.
//!
//! Registry objects are `project` entities subtyped `tool`, `skill`,
//! `plugin` or `verb`, tagged `tool-registry`. Capabilities are `concept`
//! entities subtyped `capability`; a registry object `implements` the
//! capabilities it provides. Discovery (`tool.suggest`) runs a text and
//! vector arm over the registry and a second arm over capabilities that is
//! expanded along `implements` edges, then annotates every hit with the
//! caller's policy decision. Policy and grants live in two pack-owned tables
//! (`tool_policy`, `tool_grants`).

mod handlers;
mod pack;
mod policy;
pub mod vocab;

pub use pack::ToolPack;
