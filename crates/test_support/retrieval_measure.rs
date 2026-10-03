use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use khive_storage::{SubstrateKind, TextDocument, TextSearch};
use serde_json::{json, Value};
use uuid::Uuid;

pub fn document(
    id: Uuid,
    namespace: &str,
    kind: SubstrateKind,
    record_kind: &str,
    title: String,
    body: String,
) -> TextDocument {
    TextDocument {
        subject_id: id,
        kind,
        record_kind: Some(record_kind.into()),
        namespace: namespace.into(),
        title: Some(title),
        body,
        tags: vec![],
        metadata: None,
        updated_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
    }
}

pub async fn seed_documents(
    index: &Arc<dyn TextSearch>,
    documents: impl Iterator<Item = TextDocument>,
) {
    let mut batch = Vec::with_capacity(512);
    for document in documents {
        batch.push(document);
        if batch.len() == 512 {
            let summary = index
                .upsert_documents(std::mem::take(&mut batch))
                .await
                .unwrap();
            assert_eq!(summary.failed, 0, "FTS fixture batch must be complete");
        }
    }
    if !batch.is_empty() {
        assert_eq!(index.upsert_documents(batch).await.unwrap().failed, 0);
    }
}

pub fn cache_state(first_after_reopen: bool) -> Value {
    json!({
        "process": "resident measurement test process",
        "application": if first_after_reopen { "new runtime/backend instance" } else { "resident instance after five discarded target requests" },
        "sqlite": if first_after_reopen { "new pool after constructor/schema/index setup; setup may populate page caches; no SQLite cold claim" } else { "resident pool after five discarded target requests" },
        "os_page_cache": "uncontrolled; no cold OS claim",
        "warmup_discarded_requests": if first_after_reopen { 0 } else { 5 },
        "state": if first_after_reopen { "first_target_after_reopen" } else { "resident_after_five" },
        "seeding_in_timed_interval": false,
        "target_connection_open_if_needed_in_timed_interval": true
    })
}

pub struct RawRows {
    path: PathBuf,
    writer: BufWriter<File>,
}

impl RawRows {
    pub fn new(label: &str) -> Self {
        let directory = std::env::var_os("KHIVE_MEASURE_OUTPUT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&directory).unwrap();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = directory.join(format!("{label}-{}-{nonce}.jsonl", std::process::id()));
        let writer = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap(),
        );
        println!("RETRIEVAL_MEASUREMENT_RAW_OPEN path={}", path.display());
        let mut output = Self { path, writer };
        output.push(&json!({
            "record":"measurement_plan", "label":label,
            "protocol":"scripts/perf/README.md",
            "cache_states":[cache_state(true), cache_state(false)],
            "samples_per_cell":5,
            "host_os":std::env::consts::OS, "host_arch":std::env::consts::ARCH,
            "workload":"timed reads; closed-loop batch512 seeding excluded; existing public search telemetry retained",
            "exclusive_host_window":"must be established and recorded by runner",
            "build_isolation":"must be established and recorded by runner",
            "interpretation":"exploratory; no performance PASS or CI threshold"
        }));
        output
    }

    pub fn push(&mut self, row: &Value) {
        serde_json::to_writer(&mut self.writer, row).unwrap();
        self.writer.write_all(b"\n").unwrap();
    }

    pub fn finish(mut self) {
        self.writer.flush().unwrap();
        println!("RETRIEVAL_MEASUREMENT_RAW path={}", self.path.display());
    }
}
