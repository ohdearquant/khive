//! A typed graph of printed forms: issuers, forms, sections, items, the fields
//! items mean, choice values and printed rules, with verbs to load forms,
//! decide what a new item means, resolve wordings and check a set of answers.

mod handlers;
mod pack;
mod store;
mod text;
pub(crate) mod vocab;

pub use pack::FormsPack;
