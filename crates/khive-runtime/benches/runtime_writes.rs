mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use khive_db::stores::blob::FsBlobStore;
use khive_runtime::atomic_prepare::prepare_add_entity;
use khive_runtime::keyed_memory::{create_keyed_memory, KeyedMemorySpec};
use khive_runtime::{run_atomic_unit, AtomicRunOutcome, KgArchive};
use khive_storage::{BlobStore, NewAttachment};
use khive_types::EdgeRelation;
use serde_json::json;
use support::{executor, Fixture, CONTENT, DIMS, MODEL, ROWS};

fn note_create(c: &mut Criterion) {
    let rt = executor();
    c.bench_function("runtime_create_note/10k_notes_one_stub_embedding", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let fx = Fixture::new(true);
                rt.block_on(fx.seed_notes());
                assert_eq!(
                    fx.runtime.embedding_models_for_note_kind("observation"),
                    [MODEL]
                );
                rt.block_on(fx.runtime.embed_batch(&[CONTENT.into()]))
                    .expect("warm stub");
                let start = Instant::now();
                let note = rt
                    .block_on(fx.runtime.create_note(
                        &fx.token,
                        "observation",
                        None,
                        CONTENT,
                        Some(0.5),
                        None,
                        vec![],
                    ))
                    .expect("create note");
                elapsed += start.elapsed();
                let vectors = rt
                    .block_on(
                        fx.runtime
                            .vectors(&fx.token)
                            .expect("note vector store")
                            .get_vectors(&[note.id], "local", "note.content"),
                    )
                    .expect("read created note embedding");
                assert_eq!(
                    vectors
                        .get(&note.id)
                        .expect("created note has a vector")
                        .len(),
                    DIMS
                );
                black_box(note);
            }
            elapsed
        });
    });
}

fn entity_attachments(c: &mut Criterion) {
    let rt = executor();
    let mut group = c.benchmark_group("runtime_entity_attachments");
    for count in [100, 500] {
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    let dir = tempfile::tempdir().expect("blob fixture directory");
                    let fx = Fixture::new(false);
                    let blobs = Arc::new(
                        FsBlobStore::new(dir.path().to_path_buf(), 0).expect("blob fixture"),
                    );
                    fx.runtime
                        .install_blob_store(blobs.clone())
                        .expect("install blob store");
                    let content_ref = rt
                        .block_on(blobs.put(vec![42; 1024]))
                        .expect("publish fixture blob");
                    let attachment = NewAttachment {
                        role: "content".into(),
                        content_ref,
                        media_type: None,
                        size_bytes: Some(1024),
                    };
                    let names: Vec<_> = (0..count).map(|i| format!("asset {i}")).collect();
                    let start = Instant::now();
                    rt.block_on(async {
                        for name in &names {
                            black_box(
                                fx.runtime
                                    .create_entity_with_attachments(
                                        &fx.token,
                                        "artifact",
                                        None,
                                        name,
                                        None,
                                        None,
                                        vec![],
                                        vec![attachment.clone()],
                                    )
                                    .await
                                    .expect("entity with attachment"),
                            );
                        }
                    });
                    elapsed += start.elapsed();
                }
                elapsed
            });
        });
    }
    group.finish();
}

fn atomic(c: &mut Criterion) {
    let rt = executor();
    let mut group = c.benchmark_group("runtime_atomic_unit");
    for count in [1, 3, 5] {
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    let fx = Fixture::new(false);
                    let plans = rt.block_on(async {
                        let mut plans = Vec::with_capacity(count);
                        for i in 0..count {
                            plans.push(
                                prepare_add_entity(
                                    &fx.runtime,
                                    &fx.token,
                                    &json!({"kind":"concept", "name":format!("atomic {i}")}),
                                )
                                .await
                                .expect("prepare entity"),
                            );
                        }
                        plans
                    });
                    let sql = fx.runtime.sql();
                    let start = Instant::now();
                    let result = rt
                        .block_on(run_atomic_unit(sql.as_ref(), plans))
                        .expect("atomic runner");
                    elapsed += start.elapsed();
                    assert!(
                        matches!(result, AtomicRunOutcome::Committed { .. }),
                        "atomic unit rolled back: {result:?}"
                    );
                    black_box(result);
                }
                elapsed
            });
        });
    }
    group.finish();
}

fn keyed_memory(c: &mut Criterion) {
    let rt = executor();
    let mut group = c.benchmark_group("runtime_keyed_memory_create");
    for count in [100, 500] {
        let keys: Vec<_> = (0..count).map(|i| format!("bench-key-{i}")).collect();
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    let fx = Fixture::new(false);
                    let start = Instant::now();
                    let replayed = rt.block_on(async {
                        let mut replayed = 0;
                        for key in &keys {
                            let result = create_keyed_memory(
                                &fx.runtime,
                                &fx.token,
                                KeyedMemorySpec {
                                    content: CONTENT,
                                    key,
                                    salience: 0.5,
                                    decay_factor: 0.95,
                                    properties: json!({"memory_type":"episodic"}),
                                    source_id: None,
                                    embedding_model: None,
                                },
                            )
                            .await
                            .expect("keyed create");
                            replayed += usize::from(result.2);
                            black_box(result);
                        }
                        replayed
                    });
                    elapsed += start.elapsed();
                    assert_eq!(
                        replayed, 0,
                        "fresh fixture must measure creation, not replay"
                    );
                }
                elapsed
            });
        });
    }
    group.finish();
}

fn import(c: &mut Criterion) {
    let rt = executor();
    let source = Fixture::new(false);
    // Storage seeding bypasses endpoint validation. Import must receive a
    // concept -> concept relation admitted by the live runtime contract.
    rt.block_on(source.seed_graph(EdgeRelation::Extends));
    let mut archive = rt
        .block_on(source.runtime.export_kg(&source.token))
        .expect("generate archive");
    drop(source);
    assert_eq!(archive.entities.len(), ROWS);
    assert_eq!(archive.edges.len(), ROWS);
    archive.entities.sort_unstable_by_key(|entity| entity.id);
    archive.edges.sort_unstable_by_key(|edge| edge.edge_id);
    assert!(archive
        .edges
        .iter()
        .all(|edge| edge.relation == EdgeRelation::Extends));
    let mut group = c.benchmark_group("runtime_import_kg");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("10k_entities_10k_edges", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let fx = Fixture::new(false);
                let empty = rt
                    .block_on(fx.runtime.export_kg(&fx.token))
                    .expect("inspect fresh import destination");
                assert!(empty.entities.is_empty());
                assert!(empty.edges.is_empty());
                let start = Instant::now();
                let result = rt
                    .block_on(fx.runtime.import_kg(&archive, &fx.token))
                    .expect("import");
                elapsed += start.elapsed();
                assert_eq!(result.entities_imported, ROWS);
                assert_eq!(
                    result.edges_imported,
                    ROWS,
                    "first skipped edge: {:?}",
                    result.skipped_edges.first()
                );
                assert_eq!(result.edges_skipped, 0);
                assert!(result.skipped_edges.is_empty());
                let mut persisted = rt
                    .block_on(fx.runtime.export_kg(&fx.token))
                    .expect("read back imported storage");
                assert_imported_archive(&mut persisted, &archive);
                black_box(result);
            }
            elapsed
        });
    });
    group.finish();
}

// Compare actual namespace rows, not only the import summary. Export time is
// intentionally fresh; every entity/edge archive field is preserved by import
// for this same-namespace, microsecond-timestamp fixture. Storage-only revision,
// tombstone and content-ref fields are not part of the archive contract.
fn assert_imported_archive(actual: &mut KgArchive, expected: &KgArchive) {
    assert_eq!(actual.format, expected.format);
    assert_eq!(actual.version, expected.version);
    assert_eq!(actual.namespace, expected.namespace);
    assert_eq!(actual.entities.len(), ROWS);
    assert_eq!(actual.edges.len(), ROWS);
    actual.entities.sort_unstable_by_key(|entity| entity.id);
    actual.edges.sort_unstable_by_key(|edge| edge.edge_id);
    let mut expected_entities: Vec<_> = expected.entities.iter().collect();
    let mut expected_edges: Vec<_> = expected.edges.iter().collect();
    expected_entities.sort_unstable_by_key(|entity| entity.id);
    expected_edges.sort_unstable_by_key(|edge| edge.edge_id);
    for (got, want) in actual.entities.iter().zip(expected_entities) {
        assert_eq!(got.id, want.id);
        assert_eq!(
            serde_json::to_value(got).expect("serialize persisted entity"),
            serde_json::to_value(want).expect("serialize archive entity"),
            "imported entity {}",
            want.id
        );
    }
    for (got, want) in actual.edges.iter().zip(expected_edges) {
        assert_eq!(got.edge_id, want.edge_id);
        assert_eq!(
            serde_json::to_value(got).expect("serialize persisted edge"),
            serde_json::to_value(want).expect("serialize archive edge"),
            "imported edge {}",
            want.edge_id
        );
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10).warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_secs(1));
    targets = note_create, entity_attachments, atomic, keyed_memory, import
}
criterion_main!(benches);
