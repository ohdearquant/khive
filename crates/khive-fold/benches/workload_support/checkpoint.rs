use khive_fold::{Checkpoint, CheckpointStore, FoldContext, InMemoryCheckpointStore};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub struct Fixture {
    expected: Vec<Checkpoint<Vec<i64>>>,
}
impl Fixture {
    pub fn new(count: usize) -> Self {
        let expected = (0..count)
            .map(|index| {
                let state: Vec<_> = (0..32).map(|offset| index as i64 * 32 + offset).collect();
                Checkpoint::new(
                    format!("generated:{index:04}"),
                    state,
                    Uuid::from_u128(index as u128 + 1),
                    32,
                    FoldContext::new()
                        .with_correlation_id(Uuid::from_u128(0xfeed))
                        .with_extra(serde_json::json!({"fixture":index})),
                    1,
                )
                .expect("generated checkpoint")
            })
            .collect();
        Self { expected }
    }
    pub fn sample(&self) -> Duration {
        let store = InMemoryCheckpointStore::new();
        let input = self.expected.clone();
        let mut loaded = Vec::with_capacity(input.len());
        let start = Instant::now();
        for checkpoint in input {
            store.save(checkpoint).expect("save checkpoint");
        }
        for checkpoint in &self.expected {
            loaded.push(store.load(&checkpoint.id));
        }
        let elapsed = start.elapsed();
        let expected_ids: Vec<_> = self
            .expected
            .iter()
            .map(|checkpoint| checkpoint.id.clone())
            .collect();
        assert_eq!(store.list().expect("list checkpoints"), expected_ids);
        assert_eq!(loaded.len(), self.expected.len());
        for (actual, expected) in loaded.into_iter().zip(&self.expected) {
            let actual = actual
                .expect("load checkpoint")
                .expect("saved checkpoint exists");
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.state, expected.state);
            assert_eq!(actual.uuid, expected.uuid);
            assert!(actual.hash.eq_ct(&expected.hash));
            assert_eq!(actual.entries_processed, expected.entries_processed);
            assert_eq!(actual.context.as_of, expected.context.as_of);
            assert_eq!(
                actual.context.correlation_id,
                expected.context.correlation_id
            );
            assert_eq!(actual.context.extra(), expected.context.extra());
            assert_eq!(actual.fold_version, expected.fold_version);
            assert_eq!(actual.created_at, expected.created_at);
        }
        elapsed
    }
}
