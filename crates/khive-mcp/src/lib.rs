//! khive MCP server library — exports the server, serve bootstrap, transports,
//! args, pack bootstrap, and tool parameter types for the single `request` tool.
//!
//! The binary frontend is `kkernel mcp`; this crate ships no binary of its own.

pub mod args;
mod attachment_cutover;
pub mod components;
pub mod coordinator;
#[cfg(unix)]
pub mod daemon;
pub mod pack;
pub mod pending_events;
mod request_policy;
pub mod save_sink;
pub mod serve;
pub mod server;
mod stdio_line_limit;
pub mod tools;
pub mod transport;

#[cfg(test)]
mod test_isolation;

#[cfg(feature = "bench-embedder")]
pub(crate) mod bench_embedder;
