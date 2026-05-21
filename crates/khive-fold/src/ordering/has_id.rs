//! HasId trait for candidates with stable UUID identifiers

use uuid::Uuid;

/// Trait for candidates with stable UUID identifiers.
///
/// Implementing this trait enables deterministic tie-breaking when
/// candidates have equal scores.
///
/// # Contract
///
/// - `id()` MUST return the same value for the same logical entity
/// - The UUID SHOULD be globally unique (use `Uuid::new_v4()` or similar)
/// - The UUID MUST NOT change between calls for the same instance
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
mod tests {
    use super::*;

    struct Item {
        id: Uuid,
    }

    impl HasId for Item {
        fn id(&self) -> Uuid {
            self.id
        }
    }

    #[test]
    fn test_uuid_has_id() {
        let id = Uuid::new_v4();
        assert_eq!(id.id(), id);
    }

    #[test]
    fn test_ref_has_id() {
        let item = Item { id: Uuid::new_v4() };
        let r: &Item = &item;
        assert_eq!(r.id(), item.id);
    }

    #[test]
    fn test_box_has_id() {
        let id = Uuid::new_v4();
        let boxed: Box<Uuid> = Box::new(id);
        assert_eq!(boxed.id(), id);
    }

    #[test]
    fn test_arc_has_id() {
        let id = Uuid::new_v4();
        let arc = std::sync::Arc::new(id);
        assert_eq!(arc.id(), id);
    }

    #[test]
    fn test_tuple_f64_uuid() {
        let id = Uuid::new_v4();
        let t: (f64, Uuid) = (0.5, id);
        assert_eq!(t.id(), id);
    }
}
