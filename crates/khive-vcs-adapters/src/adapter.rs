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
/// Both iterators return `Result<_, AdapterError>`. A fatal error (e.g. a
/// missing required field) stops the iterator; non-fatal warnings accumulate
/// internally and are retrievable via [`FormatAdapter::warnings`].
pub trait FormatAdapter {
    /// Short name of the format handled by this adapter (e.g. `"csv"`, `"json"`).
    fn name(&self) -> &str;

    /// Iterate over entity records in the source.
    ///
    /// The iterator returns `Ok(EntityRecord)` for each successfully parsed
    /// entity and `Err(AdapterError)` for fatal structural failures. Non-fatal
    /// issues (unknown optional fields, etc.) accumulate in [`warnings`].
    ///
    /// [`warnings`]: FormatAdapter::warnings
    fn entities(&mut self) -> impl Iterator<Item = Result<EntityRecord, AdapterError>>;

    /// Iterate over edge records in the source.
    ///
    /// Same error contract as [`entities`].
    ///
    /// [`entities`]: FormatAdapter::entities
    fn edges(&mut self) -> impl Iterator<Item = Result<EdgeRecord, AdapterError>>;

    /// Non-fatal warnings accumulated during parsing (e.g. unknown columns,
    /// missing optional fields). Empty until at least one of `entities()` or
    /// `edges()` has been driven to exhaustion.
    fn warnings(&self) -> &[String];
}
