// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Integration tests for [`JsonFormatAdapter`].
//!
//! These tests drive the public API — no access to private fields.
//! They cover the three required scenarios:
//!   1. Roundtrip: entities + edges survive parse → re-serialize
//!   2. Empty input: valid JSON empty array produces zero records
//!   3. Malformed input: invalid JSON returns a parse error, no panic
//!
//! Tests 9-12 cover the taxonomy and weight validation invariants added for KVA-AUD-001
//! and KVA-AUD-002.

use khive_vcs_adapters::{
    AdapterError, EdgeRecord, EntityRecord, FormatAdapter, JsonFormatAdapter,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const ENTITY_FIXTURE_JSON: &str = r#"[
  {
    "id": "11111111-1111-1111-1111-111111111111",
    "kind": "concept",
    "name": "FlashAttention",
    "description": "IO-aware exact attention",
    "tags": ["attention", "efficiency"],
    "properties": { "year": 2022 }
  },
  {
    "id": "22222222-2222-2222-2222-222222222222",
    "kind": "person",
    "name": "Tri Dao"
  }
]"#;

const EDGE_FIXTURE_JSON: &str = r#"[
  {
    "edge_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
    "source": "11111111-1111-1111-1111-111111111111",
    "target": "22222222-2222-2222-2222-222222222222",
    "relation": "introduced_by",
    "weight": 1.0
  }
]"#;

const MIXED_FIXTURE_JSON: &str = r#"[
  {
    "id": "11111111-1111-1111-1111-111111111111",
    "kind": "concept",
    "name": "FlashAttention"
  },
  {
    "id": "22222222-2222-2222-2222-222222222222",
    "kind": "person",
    "name": "Tri Dao"
  },
  {
    "edge_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
    "source": "11111111-1111-1111-1111-111111111111",
    "target": "22222222-2222-2222-2222-222222222222",
    "relation": "introduced_by",
    "weight": 0.9
  }
]"#;

include!("json_adapter_tests/roundtrip_and_input_tests.rs");

include!("json_adapter_tests/property_and_field_dispatch_tests.rs");

include!("json_adapter_tests/taxonomy_and_record_fidelity_tests.rs");

include!("json_adapter_tests/registered_kind_and_record_shape_tests.rs");
