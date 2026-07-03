// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Wire record shapes produced by format adapters for the KG import pipeline.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Entity record shape produced by format adapters.
///
/// Adapters produce these; the standard `khive kg import` pipeline validates
/// and loads them into `working.db`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRecord {
    pub id: Uuid,
    pub kind: String,
    /// ADR-020 pack-governed subtype token (e.g. `"paper"`). Reserved key
    /// parsed before the unknown-key fold, so it never lands in `properties`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub properties: serde_json::Value,
    #[serde(default)]
    pub tags: Vec<String>,
    /// ADR-020 RFC3339 creation timestamp. Reserved key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// ADR-020 RFC3339 last-update timestamp. Reserved key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Raw deserialization target for [`EdgeRecord`].
#[derive(Deserialize)]
struct EdgeRecordRaw {
    edge_id: Uuid,
    source: String,
    target: String,
    relation: String,
    #[serde(default = "default_weight")]
    weight: f64,
    #[serde(default)]
    properties: serde_json::Value,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

impl TryFrom<EdgeRecordRaw> for EdgeRecord {
    type Error = String;

    fn try_from(raw: EdgeRecordRaw) -> Result<Self, Self::Error> {
        if !raw.weight.is_finite() {
            return Err(format!(
                "EdgeRecord: weight must be finite, got {}",
                raw.weight
            ));
        }
        if !(0.0..=1.0).contains(&raw.weight) {
            return Err(format!(
                "EdgeRecord: weight must be in [0.0, 1.0], got {}",
                raw.weight
            ));
        }
        Ok(Self {
            edge_id: raw.edge_id,
            source: raw.source,
            target: raw.target,
            relation: raw.relation,
            weight: raw.weight,
            properties: raw.properties,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
        })
    }
}

/// Edge record shape produced by format adapters. Deserialization rejects non-finite weights.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "EdgeRecordRaw")]
pub struct EdgeRecord {
    pub edge_id: Uuid,
    pub source: String,
    pub target: String,
    pub relation: String,
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default)]
    pub properties: serde_json::Value,
    /// ADR-020 RFC3339 creation timestamp. Reserved key; not folded into `properties`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// ADR-020 RFC3339 last-update timestamp. Reserved key; not folded into `properties`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

fn default_weight() -> f64 {
    0.7
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn raw_with_weight(w: f64) -> EdgeRecordRaw {
        EdgeRecordRaw {
            edge_id: Uuid::nil(),
            source: "aa".into(),
            target: "bb".into(),
            relation: "extends".into(),
            weight: w,
            properties: serde_json::Value::Null,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn edge_record_try_from_rejects_nan_weight() {
        assert!(
            EdgeRecord::try_from(raw_with_weight(f64::NAN)).is_err(),
            "NaN weight must be rejected"
        );
    }

    #[test]
    fn edge_record_try_from_rejects_inf_weight() {
        assert!(
            EdgeRecord::try_from(raw_with_weight(f64::INFINITY)).is_err(),
            "Inf weight must be rejected"
        );
    }

    #[test]
    fn edge_record_try_from_rejects_weight_below_range() {
        assert!(
            EdgeRecord::try_from(raw_with_weight(-0.1)).is_err(),
            "weight -0.1 must be rejected (below [0.0, 1.0])"
        );
    }

    #[test]
    fn edge_record_try_from_rejects_weight_above_range() {
        assert!(
            EdgeRecord::try_from(raw_with_weight(1.1)).is_err(),
            "weight 1.1 must be rejected (above [0.0, 1.0])"
        );
    }

    #[test]
    fn edge_record_serde_rejects_weight_below_range() {
        let json = r#"{"edge_id":"00000000-0000-0000-0000-000000000000","source":"a","target":"b","relation":"extends","weight":-0.1}"#;
        let result: Result<EdgeRecord, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "serde must reject weight < 0.0 at the JSON boundary"
        );
    }

    #[test]
    fn edge_record_serde_rejects_weight_above_range() {
        let json = r#"{"edge_id":"00000000-0000-0000-0000-000000000000","source":"a","target":"b","relation":"extends","weight":2.0}"#;
        let result: Result<EdgeRecord, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "serde must reject weight > 1.0 at the JSON boundary"
        );
    }

    #[test]
    fn edge_record_try_from_accepts_finite_weight() {
        assert!(
            EdgeRecord::try_from(raw_with_weight(0.7)).is_ok(),
            "finite weight 0.7 must be accepted"
        );
    }

    #[test]
    fn edge_record_try_from_accepts_boundary_weights() {
        assert!(
            EdgeRecord::try_from(raw_with_weight(0.0)).is_ok(),
            "weight 0.0 must be accepted (lower bound)"
        );
        assert!(
            EdgeRecord::try_from(raw_with_weight(1.0)).is_ok(),
            "weight 1.0 must be accepted (upper bound)"
        );
    }

    #[test]
    fn edge_record_serde_roundtrip_valid() {
        let id = Uuid::new_v4();
        let record = EdgeRecord {
            edge_id: id,
            source: "aa".into(),
            target: "bb".into(),
            relation: "extends".into(),
            weight: 0.7,
            properties: serde_json::Value::Null,
            created_at: None,
            updated_at: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let restored: EdgeRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.edge_id, id);
        assert!((restored.weight - 0.7).abs() < 1e-12);
    }
}
