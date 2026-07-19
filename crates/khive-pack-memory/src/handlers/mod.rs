//! Memory verb handlers — split by concern.

mod common;
mod feedback;
#[cfg(test)]
mod fresh_tail_tests;
mod prune;
mod recall;
mod remember;
mod sub_handlers;
#[cfg(test)]
mod tests;

pub use common::{recall_text_terms, TextSnippetPolicy};
