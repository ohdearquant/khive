// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! The [`FormatAdapter`] trait — stateful pure transform producing entity and edge record streams.

use crate::error::AdapterError;
use crate::record::{EdgeRecord, EntityRecord};

/// A format adapter for the KG import pipeline.
///
/// Implementations parse a source format and yield entity and edge records
/// using the standard `EntityRecord`/`EdgeRecord` wire shapes. The adapter writes no database
/// state — its output is consumed by the standard `khive kg import` pipeline.
///
/// Each iterator item is `Ok(record)` on success or `Err(AdapterError)` on a per-record
/// parse failure. Non-fatal issues (unknown optional fields, etc.) accumulate internally
/// and are retrievable via [`FormatAdapter::warnings`]. Parsing may be eager or lazy
/// depending on the implementation.
pub trait FormatAdapter {
    /// Short name of the format handled by this adapter (e.g. `"csv"`, `"json"`).
    fn name(&self) -> &str;

    /// Iterate over entity records in the source.
    fn entities(&mut self) -> impl Iterator<Item = Result<EntityRecord, AdapterError>>;

    /// Iterate over edge records in the source.
    fn edges(&mut self) -> impl Iterator<Item = Result<EdgeRecord, AdapterError>>;

    /// Non-fatal warnings accumulated during parsing (e.g. unknown columns, missing optional fields).
    fn warnings(&self) -> &[String];
}
