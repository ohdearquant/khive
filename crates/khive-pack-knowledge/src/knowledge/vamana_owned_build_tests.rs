use super::*;

#[test]
fn knowledge_bridge_retains_owned_vector_buffer() {
    let vectors = vec![1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let pointer = vectors.as_ptr();
    let ids = vec![Uuid::from_u128(1), Uuid::from_u128(2)];
    let bridge = AnnBridge::build(vectors, 4, ids.clone()).unwrap();
    assert_eq!(bridge.id_map, ids);
    assert_eq!(
        bridge.index.vectors().unwrap().as_ptr(),
        pointer,
        "the knowledge bridge must transfer its normalized buffer into the index"
    );
    assert_eq!(
        bridge.index.vectors().unwrap(),
        &[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0]
    );
    assert!(!bridge
        .index
        .search(&[1.0, 0.0, 0.0, 0.0], 1)
        .unwrap()
        .is_empty());
}
