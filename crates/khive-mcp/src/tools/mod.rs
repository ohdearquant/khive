//! MCP tool parameter types.
//!
//! After v0.2 (ADR-016) the MCP surface collapses to a single `request` tool —
//! verb-specific param schemas live in the packs themselves, not here.

pub mod request;
