//! Canonical project NDJSON wire contract; no CLI or filesystem claim.

use super::{
    parse_timestamp, read_ndjson, write_edges_ndjson, write_entities_ndjson, NdjsonEdge,
    NdjsonEntity,
};
use chrono::{DateTime, Utc};
use khive_runtime::portability::{ExportedEdge, ExportedEntity};
use khive_types::EdgeRelation;
use std::io::{self, Write};
use uuid::Uuid;

const CREATED: &str = "2025-01-02T03:04:05.123456+00:00";
const UPDATED: &str = "2026-02-03T04:05:06.654321+00:00";

fn timestamp(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .unwrap()
        .with_timezone(&Utc)
}

fn entity(id: u128) -> ExportedEntity {
    ExportedEntity {
        id: Uuid::from_u128(id),
        kind: "concept".into(),
        entity_type: if id == 1 { Some("paper".into()) } else { None },
        name: format!("n{id}"),
        description: None,
        properties: Some(serde_json::json!({
            "z": [{"z": 9, "a": 1}, "keep", {"c": 3, "b": 2}],
            "a": {"y": 2, "b": 1},
        })),
        tags: vec!["z".into(), "a".into(), "a".into()],
        created_at: timestamp(CREATED),
        updated_at: timestamp(UPDATED),
    }
}

fn edge(id: u128, source: u128, target: u128, relation: EdgeRelation) -> ExportedEdge {
    ExportedEdge {
        edge_id: Uuid::from_u128(id),
        source: Uuid::from_u128(source),
        target: Uuid::from_u128(target),
        relation,
        weight: 0.75,
        properties: Some(serde_json::json!({"z": 2, "a": {"z": false, "a": true}})),
        created_at: timestamp(CREATED),
        updated_at: timestamp(UPDATED),
    }
}

#[test]
fn empty_ndjson_writers_emit_no_bytes_and_accept_dyn_write() {
    let mut bytes = Vec::new();
    let writer: &mut dyn Write = &mut bytes;
    write_entities_ndjson(&[], writer).unwrap();
    write_edges_ndjson(&[], writer).unwrap();
    assert!(bytes.is_empty());
}

#[test]
fn entity_ndjson_is_sorted_canonical_and_readable_by_sync() {
    let records = [entity(2), entity(1)];
    let original_properties = records[0].properties.clone();
    let mut bytes = Vec::new();
    write_entities_ndjson(&records, &mut bytes).unwrap();
    let mut repeated = Vec::new();
    write_entities_ndjson(&records, &mut repeated).unwrap();
    assert_eq!(repeated, bytes);
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 2);
    assert_eq!(bytes.last(), Some(&b'\n'));
    let text = std::str::from_utf8(&bytes).unwrap();
    let first = r#"{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","entity_type":"paper","name":"n1","description":null,"properties":{"a":{"b":1,"y":2},"z":[{"a":1,"z":9},"keep",{"b":2,"c":3}]},"tags":["a","a","z"],"created_at":"2025-01-02T03:04:05.123456+00:00","updated_at":"2026-02-03T04:05:06.654321+00:00"}"#;
    assert_eq!(text.lines().next().unwrap(), first);
    assert!(!text.lines().nth(1).unwrap().contains("\"entity_type\""));
    let parsed: Vec<NdjsonEntity> = read_ndjson(text, "entity").unwrap();
    assert_eq!(
        parsed.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![Uuid::from_u128(1), Uuid::from_u128(2)]
    );
    assert_eq!(parsed[0].entity_type.as_deref(), Some("paper"));
    assert_eq!(parsed[1].entity_type, None);
    assert_eq!(parsed[0].tags, vec!["a", "a", "z"]);
    assert_eq!(parsed[0].properties, records[1].properties);
    assert_eq!(
        parse_timestamp(parsed[0].created_at.as_deref(), timestamp(UPDATED)).unwrap(),
        timestamp(CREATED)
    );
    assert_eq!(
        parse_timestamp(parsed[0].updated_at.as_deref(), timestamp(CREATED)).unwrap(),
        timestamp(UPDATED)
    );
    assert_eq!(records[0].tags, vec!["z", "a", "a"]);
    assert_eq!(records[0].properties, original_properties);
    assert_eq!(records[0].id, Uuid::from_u128(2));
}

#[test]
fn entity_strings_escape_record_newlines_without_changing_the_source() {
    let mut record = entity(1);
    record.name = "quoted \"name\" — UTF-8".into();
    record.description = Some("first\nsecond".into());
    record.properties = None;
    let mut bytes = Vec::new();
    write_entities_ndjson(std::slice::from_ref(&record), &mut bytes).unwrap();
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("\"properties\":null"));
    let parsed: Vec<NdjsonEntity> = read_ndjson(text, "entity").unwrap();
    assert_eq!(parsed[0].name, record.name);
    assert_eq!(parsed[0].description, record.description);
    assert!(parsed[0].properties.is_none());
}

#[test]
fn edge_ndjson_uses_triple_order_and_preserves_identity_metadata_and_times() {
    let records = [
        edge(1, 2, 3, EdgeRelation::Extends),
        edge(4, 1, 3, EdgeRelation::Contains),
        edge(2, 1, 2, EdgeRelation::Extends),
        edge(3, 1, 2, EdgeRelation::Contains),
    ];
    let mut bytes = Vec::new();
    write_edges_ndjson(&records, &mut bytes).unwrap();
    let mut repeated = Vec::new();
    write_edges_ndjson(&records, &mut repeated).unwrap();
    assert_eq!(repeated, bytes);
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 4);
    assert_eq!(bytes.last(), Some(&b'\n'));
    let text = std::str::from_utf8(&bytes).unwrap();
    let first = r#"{"edge_id":"00000000-0000-0000-0000-000000000003","source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000002","relation":"contains","weight":0.75,"properties":{"a":{"a":true,"z":false},"z":2},"created_at":"2025-01-02T03:04:05.123456+00:00","updated_at":"2026-02-03T04:05:06.654321+00:00"}"#;
    assert_eq!(text.lines().next().unwrap(), first);
    let parsed: Vec<NdjsonEdge> = read_ndjson(text, "edge").unwrap();
    assert_eq!(
        parsed.iter().map(|row| row.edge_id).collect::<Vec<_>>(),
        [3, 2, 4, 1].map(Uuid::from_u128)
    );
    assert_eq!(parsed[0].source, records[3].source);
    assert_eq!(parsed[0].target, records[3].target);
    assert_eq!(parsed[0].relation, "contains");
    assert_eq!(parsed[0].weight, 0.75);
    assert_eq!(parsed[0].properties, records[3].properties);
    assert_eq!(
        parse_timestamp(parsed[0].created_at.as_deref(), timestamp(UPDATED)).unwrap(),
        timestamp(CREATED)
    );
    assert_eq!(
        parse_timestamp(parsed[0].updated_at.as_deref(), timestamp(CREATED)).unwrap(),
        timestamp(UPDATED)
    );
    assert_eq!(records[0].edge_id, Uuid::from_u128(1));
}

#[test]
fn edge_ndjson_rejects_nonfinite_and_out_of_range_weights_before_output() {
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 1.1] {
        let valid = edge(1, 1, 2, EdgeRelation::Contains);
        let mut bad = edge(2, 2, 3, EdgeRelation::Extends);
        bad.weight = invalid;
        let mut bytes = b"existing prefix".to_vec();
        let error = write_edges_ndjson(&[valid, bad], &mut bytes).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(bytes, b"existing prefix".to_vec());
    }
    let mut low = edge(1, 1, 2, EdgeRelation::Contains);
    let mut high = edge(2, 2, 3, EdgeRelation::Extends);
    low.weight = 0.0;
    high.weight = 1.0;
    let mut bytes = Vec::new();
    write_edges_ndjson(&[low, high], &mut bytes).unwrap();
    let parsed: Vec<NdjsonEdge> =
        read_ndjson(std::str::from_utf8(&bytes).unwrap(), "edge").unwrap();
    assert_eq!(
        parsed.iter().map(|row| row.weight).collect::<Vec<_>>(),
        [0.0, 1.0]
    );
}

#[test]
fn sync_reader_keeps_legacy_timestamp_defaults_and_required_edge_identity() {
    let entity_text = r#"{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","entity_type":null,"name":"legacy"}"#;
    let entities: Vec<NdjsonEntity> = read_ndjson(entity_text, "entity").unwrap();
    assert!(entities[0].entity_type.is_none());
    assert!(entities[0].created_at.is_none());
    assert!(entities[0].updated_at.is_none());
    assert_eq!(
        parse_timestamp(None, timestamp(CREATED)).unwrap(),
        timestamp(CREATED)
    );
    assert!(parse_timestamp(Some("invalid"), timestamp(CREATED)).is_err());
    let missing_id = r#"{"source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000002","relation":"contains"}"#;
    assert!(read_ndjson::<NdjsonEdge>(missing_id, "edge").is_err());
    let legacy_edge = r#"{"edge_id":"00000000-0000-0000-0000-000000000003","source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000002","relation":"contains"}"#;
    let edges: Vec<NdjsonEdge> = read_ndjson(legacy_edge, "edge").unwrap();
    assert_eq!(edges[0].edge_id, Uuid::from_u128(3));
    assert_eq!(edges[0].weight, 1.0);
    assert!(edges[0].created_at.is_none());
    assert!(edges[0].updated_at.is_none());
}

struct BrokenWriter;

impl Write for BrokenWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "write sentinel"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FlushFailure(Vec<u8>);

impl Write for FlushFailure {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "flush sentinel",
        ))
    }
}

#[test]
fn ndjson_writers_propagate_write_and_flush_failures() {
    let mut record = entity(1);
    record.name = "x".repeat(32 * 1024);
    let error = write_entities_ndjson(&[record], &mut BrokenWriter).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(error.to_string().contains("write sentinel"));
    let mut sink = FlushFailure::default();
    let error =
        write_edges_ndjson(&[edge(1, 1, 2, EdgeRelation::Contains)], &mut sink).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("flush sentinel"));
    assert!(!sink.0.is_empty());
}
