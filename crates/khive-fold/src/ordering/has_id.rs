//! HasId trait for candidates with stable UUID identifiers

use uuid::Uuid;

/// Stable UUID identifier for deterministic tie-breaking.
pub trait HasId {
    /// Returns the stable UUID identifier for this candidate.
    fn id(&self) -> Uuid;
}

impl HasId for Uuid {
    #[inline]
    fn id(&self) -> Uuid {
        *self
    }
}

impl HasId for (f64, Uuid) {
    #[inline]
    fn id(&self) -> Uuid {
        self.1
    }
}

impl HasId for (f32, Uuid) {
    #[inline]
    fn id(&self) -> Uuid {
        self.1
    }
}

impl<T: HasId> HasId for &T {
    #[inline]
    fn id(&self) -> Uuid {
        (*self).id()
    }
}

impl<T: HasId> HasId for &mut T {
    #[inline]
    fn id(&self) -> Uuid {
        (**self).id()
    }
}

impl<T: HasId> HasId for Box<T> {
    #[inline]
    fn id(&self) -> Uuid {
        (**self).id()
    }
}

impl<T: HasId> HasId for std::sync::Arc<T> {
    #[inline]
    fn id(&self) -> Uuid {
        (**self).id()
    }
}

#[cfg(test)]
#[path = "has_id_tests.rs"]
mod tests;
