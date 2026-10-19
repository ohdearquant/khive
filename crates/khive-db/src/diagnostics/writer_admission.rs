use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use crate::writer_admission_metrics::{HistogramSnapshot, P95Bound, Snapshot, BUCKET_EXPONENTS};

/// Bounded lifetime observations for one pool's FIFO writer, without caller identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterAdmissionDiagnostics {
    snapshot: Snapshot,
}

impl From<Snapshot> for WriterAdmissionDiagnostics {
    fn from(snapshot: Snapshot) -> Self {
        Self { snapshot }
    }
}

impl Serialize for WriterAdmissionDiagnostics {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut wire = serializer.serialize_struct("WriterAdmissionDiagnostics", 13)?;
        wire.serialize_field("admission_stage", "fifo_channel")?;
        wire.serialize_field("writer_admission_queue_depth", &self.snapshot.queue_depth)?;
        wire.serialize_field(
            "queue_depth_high_water",
            &self.snapshot.queue_depth_high_water,
        )?;
        wire.serialize_field("writer_admission_active_calls", &Option::<u64>::None)?;
        wire.serialize_field(
            "writer_admission_active_calls_unavailable_reason",
            "no_call_identity",
        )?;
        wire.serialize_field(
            "writer_admission_scheduler_wait_seconds",
            &Option::<()>::None,
        )?;
        wire.serialize_field(
            "writer_admission_scheduler_wait_seconds_unavailable_reason",
            "no_rotation_stage",
        )?;
        wire.serialize_field(
            "writer_admission_wait_seconds",
            &HistogramWire(&self.snapshot.admission_wait),
        )?;
        wire.serialize_field(
            "writer_admission_accepted_queue_age_seconds",
            &HistogramWire(&self.snapshot.accepted_queue_age),
        )?;
        wire.serialize_field(
            "writer_admission_service_seconds",
            &HistogramWire(&self.snapshot.service),
        )?;
        wire.serialize_field(
            "writer_admission_saturation_total",
            &self.snapshot.saturation_total,
        )?;
        wire.serialize_field(
            "writer_admission_saturation_reason",
            "writer_queue_saturated",
        )?;
        wire.serialize_field("writer_admission_saturation_stage", "channel_capacity")?;
        wire.end()
    }
}

#[derive(Serialize)]
struct BucketWire {
    le_seconds: f64,
    count: u64,
}

struct HistogramWire<'a>(&'a HistogramSnapshot);

impl Serialize for HistogramWire<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let buckets = std::array::from_fn::<_, 18, _>(|index| BucketWire {
            le_seconds: 2_f64.powi(i32::from(BUCKET_EXPONENTS[index])),
            count: self.0.buckets[index],
        });
        let (p95, reason) = match self.0.p95 {
            P95Bound::NoSamples => (None, Some("no_samples")),
            P95Bound::Overflow => (None, Some("overflow_bucket")),
            P95Bound::Finite(index) => (Some(2_f64.powi(i32::from(BUCKET_EXPONENTS[index]))), None),
        };
        let mut wire = serializer.serialize_struct("WriterAdmissionHistogram", 6)?;
        wire.serialize_field("buckets", &buckets)?;
        wire.serialize_field("overflow_count", &self.0.overflow_count)?;
        wire.serialize_field("count", &self.0.count)?;
        wire.serialize_field("sum_seconds", &self.0.sum.as_secs_f64())?;
        wire.serialize_field("p95_le_seconds", &p95)?;
        wire.serialize_field("p95_le_seconds_unavailable_reason", &reason)?;
        wire.end()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::Poll;
    use std::time::Duration;

    use super::*;
    use crate::diagnostics::WriterContentionDiagnostics;
    use crate::{ConnectionPool, PoolConfig};

    #[tokio::test]
    async fn snapshot_does_not_start_writer_and_keeps_pool_lifetimes_separate() {
        let directory = tempfile::tempdir().unwrap();
        let config = PoolConfig {
            path: Some(directory.path().join("admission.db")),
            write_queue_enabled: Some(true),
            volume_lock_dir: Some(directory.path().join("volume-locks")),
            ..PoolConfig::for_test()
        };
        let first = ConnectionPool::new(config.clone()).unwrap();
        let second = ConnectionPool::new(config).unwrap();
        for pool in [&first, &second] {
            let before = pool.writer_acquisition_snapshot();
            let wire =
                serde_json::to_value(WriterContentionDiagnostics::snapshot(pool, None, None))
                    .unwrap();
            assert!(wire["writer_admission"].is_null());
            assert_eq!(
                wire["writer_admission_unavailable_reason"],
                "writer_not_initialized"
            );
            assert_eq!(pool.writer_task_spawn_count(), 0);
            assert_eq!(pool.writer_acquisition_snapshot(), before);
        }
        let handle = first.writer_task_handle().unwrap().unwrap();
        let empty = serde_json::to_value(WriterContentionDiagnostics::snapshot(&first, None, None))
            .unwrap();
        let empty = &empty["writer_admission"];
        assert_eq!(empty["admission_stage"], "fifo_channel");
        assert!(empty["writer_admission_active_calls"].is_null());
        assert_eq!(
            empty["writer_admission_active_calls_unavailable_reason"],
            "no_call_identity"
        );
        assert!(empty["writer_admission_scheduler_wait_seconds"].is_null());
        assert_eq!(
            empty["writer_admission_scheduler_wait_seconds_unavailable_reason"],
            "no_rotation_stage"
        );
        let histogram = &empty["writer_admission_wait_seconds"];
        assert_eq!(histogram["count"], 0);
        assert!(histogram["p95_le_seconds"].is_null());
        assert_eq!(histogram["p95_le_seconds_unavailable_reason"], "no_samples");
        assert_eq!(histogram["buckets"].as_array().unwrap().len(), 18);
        assert_eq!(histogram["buckets"][0]["le_seconds"], 0.000244140625);
        assert_eq!(histogram["buckets"][17]["le_seconds"], 32.0);
        assert_eq!(
            handle
                .send(|conn| Ok::<_, khive_storage::StorageError>(
                    conn.query_row("SELECT 7", [], |row| row.get::<_, i64>(0))
                        .unwrap()
                ))
                .await
                .unwrap(),
            7
        );
        let observed =
            serde_json::to_value(WriterContentionDiagnostics::snapshot(&first, None, None))
                .unwrap();
        let observed = &observed["writer_admission"];
        assert_eq!(observed["writer_admission_queue_depth"], 0);
        assert_eq!(observed["writer_admission_service_seconds"]["count"], 1);
        assert_eq!(second.writer_task_spawn_count(), 0);
        let second_handle = second.writer_task_handle().unwrap().unwrap();
        assert_eq!(
            second_handle
                .admission_snapshot()
                .unwrap()
                .admission_wait
                .count,
            0
        );
        assert_eq!(handle.admission_snapshot().unwrap().admission_wait.count, 1);
        for _ in 0..2 {
            let memory = ConnectionPool::new(PoolConfig {
                path: None,
                write_queue_enabled: Some(false),
                ..PoolConfig::for_test()
            })
            .unwrap();
            let wire =
                serde_json::to_value(WriterContentionDiagnostics::snapshot(&memory, None, None))
                    .unwrap();
            assert!(wire["writer_admission"].is_null());
            assert_eq!(
                wire["writer_admission_unavailable_reason"],
                "writer_queue_disabled"
            );
            assert_eq!(memory.writer_task_spawn_count(), 0);
        }
    }

    #[tokio::test]
    async fn public_snapshot_tracks_held_backlog_and_settled_rows() {
        let directory = tempfile::tempdir().unwrap();
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(directory.path().join("backlog.db")),
            write_queue_enabled: Some(true),
            write_queue_capacity: 1,
            volume_lock_dir: Some(directory.path().join("volume-locks")),
            ..PoolConfig::for_test()
        })
        .unwrap();
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch("CREATE TABLE witness(value INTEGER)")
            .unwrap();
        let handle = pool.writer_task_handle().unwrap().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_handle = handle.clone();
        let first = tokio::spawn(async move {
            first_handle
                .send(move |conn| {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    conn.execute("INSERT INTO witness VALUES (1)", [])
                        .map_err(|error| khive_storage::StorageError::Internal(error.to_string()))
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .unwrap()
            .unwrap();
        let mut second = Box::pin(handle.send(|conn| {
            conn.execute("INSERT INTO witness VALUES (2)", [])
                .map_err(|error| khive_storage::StorageError::Internal(error.to_string()))
        }));
        std::future::poll_fn(|cx| {
            assert!(second.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let before = pool.writer_acquisition_snapshot();
        let held =
            serde_json::to_value(WriterContentionDiagnostics::snapshot(&pool, None, None)).unwrap();
        assert_eq!(pool.writer_acquisition_snapshot(), before);
        assert_eq!(pool.writer_task_spawn_count(), 1);
        let held = &held["writer_admission"];
        assert_eq!(held["writer_admission_queue_depth"], 1);
        assert_eq!(held["queue_depth_high_water"], 1);
        assert_eq!(held["writer_admission_wait_seconds"]["count"], 2);
        assert_eq!(
            held["writer_admission_accepted_queue_age_seconds"]["count"],
            1
        );
        assert_eq!(held["writer_admission_service_seconds"]["count"], 0);
        assert_eq!(held["writer_admission_saturation_total"], 0);
        release_tx.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), first)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            1
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), second)
                .await
                .unwrap()
                .unwrap(),
            1
        );
        let settled =
            serde_json::to_value(WriterContentionDiagnostics::snapshot(&pool, None, None)).unwrap();
        let settled = &settled["writer_admission"];
        assert_eq!(settled["writer_admission_queue_depth"], 0);
        assert_eq!(settled["queue_depth_high_water"], 1);
        for field in [
            "writer_admission_wait_seconds",
            "writer_admission_accepted_queue_age_seconds",
            "writer_admission_service_seconds",
        ] {
            let histogram = &settled[field];
            assert_eq!(histogram["count"], 2);
            let finite = histogram["buckets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|bucket| bucket["count"].as_u64().unwrap())
                .sum::<u64>();
            assert_eq!(finite + histogram["overflow_count"].as_u64().unwrap(), 2);
        }
        let rows: (i64, i64) = pool
            .reader()
            .unwrap()
            .query_row("SELECT COUNT(*), SUM(value) FROM witness", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(rows, (2, 3));
    }

    #[test]
    fn histogram_wire_uses_bucket_bounds_and_reports_overflow_without_nonfinite_numbers() {
        let mut histogram = HistogramSnapshot {
            buckets: [0; 18],
            overflow_count: 1,
            count: 1,
            sum: std::time::Duration::from_secs(40),
            p95: P95Bound::Overflow,
        };
        let overflow = serde_json::to_value(HistogramWire(&histogram)).unwrap();
        assert_eq!(overflow["sum_seconds"], 40.0);
        assert_eq!(overflow["overflow_count"], 1);
        assert!(overflow["p95_le_seconds"].is_null());
        assert_eq!(
            overflow["p95_le_seconds_unavailable_reason"],
            "overflow_bucket"
        );
        histogram.buckets[12] = 19;
        histogram.count = 20;
        histogram.sum += std::time::Duration::from_secs(19);
        histogram.p95 = P95Bound::Finite(12);
        let finite = serde_json::to_value(HistogramWire(&histogram)).unwrap();
        assert_eq!(finite["p95_le_seconds"], 1.0);
        assert!(finite["p95_le_seconds_unavailable_reason"].is_null());
        assert_eq!(finite["sum_seconds"], 59.0);
        let count = finite["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|bucket| bucket["count"].as_u64().unwrap())
            .sum::<u64>();
        assert_eq!(count + finite["overflow_count"].as_u64().unwrap(), 20);
    }
}
