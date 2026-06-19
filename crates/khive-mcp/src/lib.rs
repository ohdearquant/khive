//! khive MCP server library — exports the server, serve bootstrap, transports,
//! args, pack bootstrap, and tool parameter types for the single `request` tool.
//!
//! The binary frontend is `kkernel mcp`; this crate ships no binary of its own.

pub mod args;
pub mod coordinator;
#[cfg(unix)]
pub mod daemon;
pub mod pack;
pub mod serve;
pub mod server;
pub mod tools;
pub mod transport;

#[cfg(feature = "bench-embedder")]
pub(crate) mod bench_embedder;
