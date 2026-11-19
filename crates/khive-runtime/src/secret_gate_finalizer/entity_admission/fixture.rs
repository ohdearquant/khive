//! Non-deployable, namespace-scoped admission and observation fixture.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use super::super::log_sink::FinalizerAuditGap;
use super::super::outcome::FinalizerOutcome;
use super::ManifestSnapshot;

struct FixtureState {
    snapshot: Arc<ManifestSnapshot>,
    owner: Arc<()>,
    outcomes: Vec<FinalizerOutcome>,
    gaps: Vec<FinalizerAuditGap>,
}

static FIXTURES: LazyLock<Mutex<HashMap<String, FixtureState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) struct ManifestFixtureGuard {
    namespace: String,
    owner: Arc<()>,
}

impl Drop for ManifestFixtureGuard {
    fn drop(&mut self) {
        let mut fixtures = FIXTURES.lock().unwrap();
        if fixtures
            .get(&self.namespace)
            .is_some_and(|state| Arc::ptr_eq(&state.owner, &self.owner))
        {
            fixtures.remove(&self.namespace);
        }
    }
}

/// This fixture is not evidence of operator adjudication.
pub(crate) fn install(namespace: &str, snapshot: Arc<ManifestSnapshot>) -> ManifestFixtureGuard {
    let owner = Arc::new(());
    let mut fixtures = FIXTURES.lock().unwrap();
    assert!(
        !fixtures.contains_key(namespace),
        "manifest fixture namespace already installed"
    );
    assert!(fixtures.len() < 64, "manifest fixture capacity exceeded");
    fixtures.insert(
        namespace.into(),
        FixtureState {
            snapshot,
            owner: owner.clone(),
            outcomes: Vec::new(),
            gaps: Vec::new(),
        },
    );
    ManifestFixtureGuard {
        namespace: namespace.into(),
        owner,
    }
}

pub(super) fn snapshot(namespace: &str) -> Arc<ManifestSnapshot> {
    FIXTURES
        .lock()
        .unwrap()
        .get(namespace)
        .map(|state| state.snapshot.clone())
        .unwrap_or_else(ManifestSnapshot::empty)
}

pub(crate) fn outcomes(namespace: &str) -> Vec<FinalizerOutcome> {
    FIXTURES
        .lock()
        .unwrap()
        .get(namespace)
        .map(|state| state.outcomes.clone())
        .unwrap_or_default()
}

pub(crate) fn audit_gaps(namespace: &str) -> Vec<FinalizerAuditGap> {
    FIXTURES
        .lock()
        .unwrap()
        .get(namespace)
        .map(|state| state.gaps.clone())
        .unwrap_or_default()
}

pub(super) fn record_outcome(namespace: &str, outcome: FinalizerOutcome) {
    if let Some(state) = FIXTURES.lock().unwrap().get_mut(namespace) {
        state.outcomes.push(outcome);
    }
}

pub(crate) fn record_gap(gap: FinalizerAuditGap) {
    if let Some(state) = FIXTURES.lock().unwrap().get_mut(&gap.namespace) {
        state.gaps.push(gap);
    }
}
