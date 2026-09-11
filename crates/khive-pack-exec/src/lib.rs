//! `exec` pack (ADR-181): materialize a tree of blob references into a
//! sandbox directory, run one registered tool under `tool.check`, capture
//! stdout, stderr and changed files back to the blob store, and write one
//! durable receipt per call, refusals included.
//!
//! The sandbox is macOS seatbelt (`sandbox-exec`) with a deny-default profile
//! rendered from a fixed template; see [`sandbox`].

mod capture;
mod handlers;
mod pack;
mod receipts;
pub mod sandbox;
pub mod tree;
pub mod vocab;

pub use pack::ExecPack;
