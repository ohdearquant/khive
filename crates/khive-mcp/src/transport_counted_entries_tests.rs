use std::cell::Cell;

use super::{HashMap, OutstandingRequest, RequestId};

// Every method that reaches an element counts it; a traversal method,
// should one be added, must count each element it yields.
#[derive(Default)]
pub(super) struct OutstandingEntries {
    inner: HashMap<RequestId, OutstandingRequest>,
    element_touches: Cell<usize>,
}

impl OutstandingEntries {
    fn record_element_touch(&self) {
        self.element_touches.set(self.element_touches.get() + 1);
    }

    pub(super) fn contains_key(&self, id: &RequestId) -> bool {
        self.record_element_touch();
        self.inner.contains_key(id)
    }

    pub(super) fn get(&self, id: &RequestId) -> Option<&OutstandingRequest> {
        self.record_element_touch();
        self.inner.get(id)
    }

    pub(super) fn get_mut(&mut self, id: &RequestId) -> Option<&mut OutstandingRequest> {
        self.record_element_touch();
        self.inner.get_mut(id)
    }

    pub(super) fn remove(&mut self, id: &RequestId) -> Option<OutstandingRequest> {
        self.record_element_touch();
        self.inner.remove(id)
    }

    pub(super) fn insert(
        &mut self,
        id: RequestId,
        obligation: OutstandingRequest,
    ) -> Option<OutstandingRequest> {
        self.record_element_touch();
        self.inner.insert(id, obligation)
    }

    pub(super) fn len(&self) -> usize {
        self.inner.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub(super) fn take_element_touches(&self) -> usize {
        self.element_touches.replace(0)
    }
}
