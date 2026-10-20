use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_mcp::coordinator::CoordinatorService;
use khive_runtime::{BackendId, KhiveRuntime, RuntimeConfig};
use khive_storage::{Entity, EntityFilter};
use khive_types::Namespace;
use kkernel::coordinator::{
    BackendRegistry, LocatorCache, SubstrateCoordinator, SubstrateCoordinatorService,
};

use crate::routing_support::{id, Workload};

#[derive(Clone, Copy)]
pub enum Operation {
    Hit,
    Miss,
    Unknown,
}

pub struct LocateCase {
    service: SubstrateCoordinatorService,
    cache: Arc<LocatorCache>,
    executor: tokio::runtime::Runtime,
    backends: [BackendId; 2],
    count: usize,
    next: u64,
    operation: Operation,
}

impl LocateCase {
    pub fn new(count: usize, operation: Operation) -> Self {
        assert_eq!(count % 2, 0);
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(2)
            .build()
            .expect("bounded benchmark executor");
        let backends = [BackendId::main(), BackendId::parse("secondary").unwrap()];
        let coordinator = executor.block_on(async {
            let mut registry = BackendRegistry::new();
            for (slot, backend) in backends.iter().enumerate() {
                let runtime = Arc::new(
                    KhiveRuntime::new(RuntimeConfig {
                        db_path: None,
                        backend_id: backend.clone(),
                        packs: vec!["kg".into()],
                        actor_id: Some("benchmark-locate".into()),
                        brain_profile: None,
                        ..RuntimeConfig::no_embeddings()
                    })
                    .expect("isolated in-memory backend"),
                );
                assert!(runtime.registered_embedding_model_names().is_empty());
                let token = runtime
                    .authorize(Namespace::local())
                    .expect("fixture token");
                let store = runtime.entities(&token).expect("entity store");
                let entities = (0..count)
                    .filter(|index| index % 2 == slot)
                    .map(|index| {
                        Entity::minimal(
                            id(index as u64 + 1),
                            "local",
                            "concept",
                            format!("routing entity {index}"),
                            1_700_000_000_000_000,
                            1_700_000_000_000_000,
                        )
                    })
                    .collect();
                let receipt = store.upsert_entities(entities).await.expect("seed backend");
                assert_eq!(receipt.attempted, (count / 2) as u64);
                assert_eq!(receipt.affected, (count / 2) as u64);
                assert_eq!(receipt.failed, 0);
                assert_eq!(
                    store
                        .count_entities("local", EntityFilter::default())
                        .await
                        .expect("count fixture"),
                    (count / 2) as u64
                );
                assert!(registry.register(backend.clone(), runtime));
            }
            assert_eq!(registry.len(), 2);
            SubstrateCoordinator::new(registry)
        });
        let cache = Arc::clone(coordinator.locator_cache());
        let service = SubstrateCoordinatorService::new(coordinator);
        assert_eq!(service.primary_backend_id(), Some(backends[0].clone()));
        assert!(!service.is_single_backend());
        for index in 0..count {
            let key = id(index as u64 + 1);
            let expected = &backends[index % 2];
            assert_eq!(
                executor.block_on(service.locate(key)).as_ref(),
                Some(expected)
            );
            assert_eq!(cache.get(key).as_ref(), Some(expected));
        }
        assert_eq!(cache.len(), count);
        assert!(executor.block_on(service.locate(id(u64::MAX))).is_none());
        Self {
            service,
            cache,
            executor,
            backends,
            count,
            next: 0,
            operation,
        }
    }
}

impl Workload for LocateCase {
    fn sample(&mut self) -> Duration {
        let index = self.next as usize % self.count;
        self.next = self.next.checked_add(1).expect("bounded probe sequence");
        let key = match self.operation {
            Operation::Unknown => id(u64::MAX),
            Operation::Hit | Operation::Miss => id(index as u64 + 1),
        };
        let expected = match self.operation {
            Operation::Unknown => None,
            Operation::Hit | Operation::Miss => Some(&self.backends[index % 2]),
        };
        match self.operation {
            Operation::Hit => {
                self.service
                    .record_created(key, self.backends[index % 2].clone());
                assert_eq!(self.cache.get(key).as_ref(), expected);
            }
            Operation::Miss | Operation::Unknown => {
                self.cache.remove(key);
                assert!(self.cache.get(key).is_none());
            }
        }
        let started = Instant::now();
        let result = black_box(self.executor.block_on(self.service.locate(black_box(key))));
        let elapsed = started.elapsed();
        assert_eq!(result.as_ref(), expected);
        assert_eq!(self.cache.get(key).as_ref(), expected);
        elapsed
    }
}
