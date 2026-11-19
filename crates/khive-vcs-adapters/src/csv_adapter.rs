// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Header-based CSV and TSV adapters. See `docs/api/csv-tsv-adapter.md`.

use std::collections::HashSet;

use khive_types::ImportKindPolicy;
use serde_json::{Map, Value};

use crate::json_adapter::{parse_edge, parse_entity};
use crate::{AdapterError, EdgeRecord, EntityRecord, FormatAdapter};

/// Delimiter and advertised name for a tabular input.
#[derive(Clone, Copy, Debug)]
pub enum DelimitedFormat {
    Csv,
    Tsv,
}

/// Pure, eager tabular transform using the same record validation as JSON imports.
pub struct CsvFormatAdapter {
    format: DelimitedFormat,
    entities: Vec<Result<EntityRecord, AdapterError>>,
    edges: Vec<Result<EdgeRecord, AdapterError>>,
    warnings: Vec<String>,
}

impl CsvFormatAdapter {
    /// Read headers and records, accepting the supplied pack-defined entity kinds.
    ///
    /// `default_kind` supplies missing/blank entity kinds. An empty registry accepts
    /// only base kinds and aliases. Record failures remain in the corresponding
    /// iterator; structural failures return immediately. Callers must validate both
    /// streams completely before writing. See `docs/api/csv-tsv-adapter.md`.
    pub fn new(
        source: &str,
        format: DelimitedFormat,
        default_kind: Option<&str>,
        extra_valid_kinds: &[String],
    ) -> Result<Self, AdapterError> {
        Self::new_with_kind_policy(
            source,
            format,
            default_kind,
            extra_valid_kinds,
            ImportKindPolicy::Strict,
        )
    }

    /// Parse tabular records with explicit unknown-entity-kind admission.
    /// Relations and every other record check are unchanged.
    pub fn new_with_kind_policy(
        source: &str,
        format: DelimitedFormat,
        default_kind: Option<&str>,
        extra_valid_kinds: &[String],
        policy: ImportKindPolicy,
    ) -> Result<Self, AdapterError> {
        let delimiter = match format {
            DelimitedFormat::Csv => b',',
            DelimitedFormat::Tsv => b'\t',
        };
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .flexible(false)
            .from_reader(source.as_bytes());
        let headers: Vec<String> = reader
            .headers()
            .map_err(|error| AdapterError::Parse(error.to_string()))?
            .iter()
            .map(|header| header.trim().to_owned())
            .collect();
        if headers.is_empty() {
            return Err(AdapterError::Parse("CSV/TSV has no header row".into()));
        }
        let mut names = HashSet::new();
        for header in &headers {
            if header.is_empty() || !names.insert(header.to_ascii_lowercase()) {
                return Err(AdapterError::Parse(format!(
                    "empty or duplicate CSV/TSV header {header:?}"
                )));
            }
        }
        let edge_list = names.contains("source") && names.contains("target");
        let required = if edge_list { "relation" } else { "name" };
        if !names.contains(required) {
            return Err(AdapterError::Parse(format!(
                "CSV/TSV missing required column '{required}'"
            )));
        }
        if !edge_list && !names.contains("kind") && default_kind.is_none() {
            return Err(AdapterError::Parse(
                "entity CSV/TSV requires a kind column or --default-kind".into(),
            ));
        }

        let mut adapter = Self {
            format,
            entities: Vec::new(),
            edges: Vec::new(),
            warnings: Vec::new(),
        };
        for (index, row) in reader.records().enumerate() {
            let row = row.map_err(|error| AdapterError::Parse(error.to_string()))?;
            let object = row_object(index, &headers, &row, edge_list, default_kind);
            if edge_list {
                adapter.edges.push(
                    object.and_then(|object| parse_edge(index, object, &mut adapter.warnings)),
                );
            } else {
                adapter.entities.push(object.and_then(|object| {
                    parse_entity(
                        index,
                        object,
                        &mut adapter.warnings,
                        extra_valid_kinds,
                        policy,
                    )
                }));
            }
        }
        Ok(adapter)
    }
}

fn row_object(
    index: usize,
    headers: &[String],
    row: &csv::StringRecord,
    edge_list: bool,
    default_kind: Option<&str>,
) -> Result<Map<String, Value>, AdapterError> {
    let mut object = Map::new();
    for (header, cell) in headers.iter().zip(row.iter()) {
        let name = header.to_ascii_lowercase();
        let trimmed = cell.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = if name == "properties" || (!edge_list && name == "tags") {
            let value: Value =
                serde_json::from_str(cell).map_err(|error| AdapterError::InvalidField {
                    index,
                    field: name.clone(),
                    reason: error.to_string(),
                })?;
            let valid = if name == "properties" {
                value.is_object()
            } else {
                value
                    .as_array()
                    .is_some_and(|values| values.iter().all(Value::is_string))
            };
            if !valid {
                return Err(AdapterError::InvalidField {
                    index,
                    field: name,
                    reason: "expected a JSON object for properties or a JSON string array for tags"
                        .into(),
                });
            }
            value
        } else if edge_list && name == "weight" {
            let weight: f64 = trimmed.parse().map_err(|_| AdapterError::InvalidField {
                index,
                field: name.clone(),
                reason: "must be a finite number in [0.0, 1.0]".into(),
            })?;
            Value::Number(serde_json::Number::from_f64(weight).ok_or_else(|| {
                AdapterError::InvalidField {
                    index,
                    field: name,
                    reason: "must be a finite number in [0.0, 1.0]".into(),
                }
            })?)
        } else {
            Value::String(cell.to_owned())
        };
        object.insert(header.clone(), value);
    }
    if !edge_list && !object.keys().any(|key| key.eq_ignore_ascii_case("kind")) {
        if let Some(kind) = default_kind {
            object.insert("kind".into(), Value::String(kind.to_owned()));
        }
    }
    Ok(object)
}

impl FormatAdapter for CsvFormatAdapter {
    fn name(&self) -> &str {
        match self.format {
            DelimitedFormat::Csv => "csv",
            DelimitedFormat::Tsv => "tsv",
        }
    }

    fn entities(&mut self) -> impl Iterator<Item = Result<EntityRecord, AdapterError>> {
        self.entities.drain(..)
    }

    fn edges(&mut self) -> impl Iterator<Item = Result<EdgeRecord, AdapterError>> {
        self.edges.drain(..)
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
