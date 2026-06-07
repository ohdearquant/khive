//! Recursive-descent parser for the verb-dispatch DSL.

mod dispatch;
mod parser_impl;
mod path;
mod scan;

pub use dispatch::parse_request;

pub(crate) use path::{apply_path_segment, split_path};
