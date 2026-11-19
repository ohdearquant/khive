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
