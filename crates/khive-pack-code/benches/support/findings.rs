use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use khive_pack_code::{ClippyProvenance, CodeIngestBatch, CodeIngestOptions};
use serde_json::json;
use uuid::Uuid;

pub const SIZES: [usize; 2] = [100, 10_000];

pub fn options() -> CodeIngestOptions<'static> {
    let observed_at: DateTime<Utc> = "2026-10-17T00:00:00Z".parse().expect("fixed timestamp");
    CodeIngestOptions {
        namespace: "local",
        observed_at,
        source_run: Some("benchmark-generated-v1"),
    }
}

pub fn provenance() -> ClippyProvenance<'static> {
    ClippyProvenance {
        repo: "generated",
        branch: "main",
        commit: "benchmark-v1",
        scope: "generated-crate",
    }
}

pub fn findings(count: usize) -> Vec<u8> {
    assert!(SIZES.contains(&count));
    let rows: Vec<_> = (0..count).map(|i| json!({
        "id": format!("F{i:05}"), "title": format!("Generated finding {i}"),
        "severity":"medium", "confidence":"high", "failure_scenario": format!("Generated case {i}"),
        "evidence":[{"path":format!("src/unit_{i:05}.rs"), "line": i+1}],
        "recommendation":"Generated improvement", "standard":"generated-rule"
    })).collect();
    serde_json::to_vec(
        &json!({"audit":{"date":"2026-10-17","scope":"generated-crate",
        "repo":"generated","branch":"main","commit":"benchmark-v1","standards_file":"generated.md"},
        "findings":rows}),
    )
    .expect("generated findings JSON")
}

pub fn clippy(count: usize) -> Vec<u8> {
    assert!(SIZES.contains(&count));
    let mut input = Vec::new();
    for i in 0..count {
        let row = json!({"reason":"compiler-message","package_id":"generated 0.1.0",
            "message":{"message":format!("generated warning {i}"),"code":{"code":"clippy::needless_borrow"},
                "level":"warning", "spans":[{"file_name":format!("src/unit_{i:05}.rs"),
                    "line_start":1,"line_end":1,"column_start":1,"column_end":12,"is_primary":true,
                    "text":[{"text":format!("consume(&value_{i});"),"highlight_start":1,"highlight_end":12}]}],
                "children":[]}});
        serde_json::to_writer(&mut input, &row).expect("generated diagnostic");
        input.push(b'\n');
    }
    serde_json::to_writer(
        &mut input,
        &json!({"reason":"build-finished","success":true}),
    )
    .expect("terminal marker");
    input.push(b'\n');
    input
}

pub fn identity(batch: &CodeIngestBatch, count: usize) -> Vec<Uuid> {
    assert_eq!(batch.entities.len(), 1);
    assert_eq!(batch.notes.len(), count);
    assert_eq!(batch.edges.len(), count);
    let ids: Vec<_> = batch.notes.iter().map(|note| note.id).collect();
    assert_eq!(ids.iter().copied().collect::<BTreeSet<_>>().len(), count);
    for (note, edge) in batch.notes.iter().zip(&batch.edges) {
        assert_eq!(note.kind, "finding");
        assert_eq!(note.namespace, "local");
        assert_eq!(edge.source_id, note.id);
        assert_eq!(edge.target_id, batch.entities[0].id);
        assert_eq!(edge.relation, khive_types::EdgeRelation::Annotates);
    }
    ids
}
