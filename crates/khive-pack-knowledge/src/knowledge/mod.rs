//! Knowledge corpus handlers — atoms, domains, TF-IDF search, fold, index.

pub(crate) mod matching;
pub(crate) mod schema;
pub(crate) mod section_feedback;
pub(crate) mod vamana;

mod compose;
mod crud;
mod fold_handler;
mod index_handler;
mod search;
mod sections;
pub(crate) mod sections_index;
pub(super) mod util;
pub(crate) struct KnowledgeHandlers;
