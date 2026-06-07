//! Memory verb handlers — split by concern.

mod common;
mod feedback;
mod recall;
mod remember;
mod sub_handlers;
#[cfg(test)]
mod tests;

pub use common::{recall_text_terms, TextSnippetPolicy};
