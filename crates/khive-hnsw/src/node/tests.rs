use super::*;

#[test]
fn test_node_creation() {
    let vector = vec![3.0, 4.0]; // norm = 5.0
    let node = HnswNode::new(vector, 2);

    assert_eq!(node.max_layer, 2);
    assert!((node.norm - 5.0).abs() < 0.001);
    assert_eq!(node.neighbors.len(), 3); // layers 0, 1, 2
}

#[test]
fn test_node_update_vector() {
    let mut node = HnswNode::new(vec![1.0, 0.0], 1);
    assert!((node.norm - 1.0).abs() < 0.001);

    node.update_vector(vec![3.0, 4.0]);
    assert!((node.norm - 5.0).abs() < 0.001);
    assert_eq!(node.vector, vec![3.0, 4.0]);
}
