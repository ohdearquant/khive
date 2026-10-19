use khive_fold::{AnchorGraph, AnchorRef};
use uuid::Uuid;

const SEED: u128 = 0xfeed_0000;
pub struct Fixture {
    pub graph: AnchorGraph,
}
impl Fixture {
    pub fn new(count: usize) -> Self {
        let mut graph = AnchorGraph::new();
        for index in 0..count {
            graph.add_node(AnchorRef {
                id: Uuid::from_u128(SEED + index as u128),
                kind: "generated".to_string(),
                stable_id: Some(format!("node-{index:04}")),
            });
        }
        for index in 0..count {
            for step in [1, 2] {
                if index + step < count {
                    graph.add_edge(
                        graph.nodes[index].id,
                        graph.nodes[index + step].id,
                        "derives_from",
                    );
                }
            }
        }
        assert_eq!(graph.nodes.len(), count);
        assert_eq!(graph.edges.len(), 2 * count - 3);
        Self { graph }
    }
    pub fn validate_trace(&self, mut result: Vec<AnchorRef>) {
        result.sort_by_key(|node| node.id);
        assert_eq!(result, self.graph.nodes[1..]);
    }
    pub fn validate_credit(&self, mut result: Vec<(AnchorRef, f64)>) {
        let count = self.graph.nodes.len();
        result.sort_by_key(|(node, _)| node.id);
        assert_eq!(result.len(), count - 1);
        for (index, (node, weight)) in result.into_iter().enumerate() {
            assert_eq!(node, self.graph.nodes[index]);
            let distance = (count - 1 - index).div_ceil(2);
            assert_eq!(weight, 2_f64.powi(-(distance as i32)));
        }
    }
}
