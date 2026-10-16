//! Authorized graph observations and guarded lint remedies (ADR-048).
mod types;
pub use types::*;
mod capture;
mod fix;
pub use capture::{InspectedLintEdge, LintCapture};
#[cfg(test)]
mod tests;
