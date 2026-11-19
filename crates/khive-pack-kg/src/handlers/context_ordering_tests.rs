use super::*;

fn ordered_cases() -> [(f64, u128, u128); 12] {
    [
        (f64::INFINITY, 90, 1),
        (0.5 + 2.0_f64.powi(-40), 80, 1),
        (0.5, 70, 1),
        (0.25, 60, 9),
        (0.25, 61, 2),
        (0.25, 61, 3),
        (0.0, 50, 1),
        (-0.0, 1, 1),
        (f64::NEG_INFINITY, 40, 1),
        (f64::from_bits(0xfff8_0000_0000_0001), 0, 1),
        (f64::from_bits(0x7ff8_0000_0000_0002), 2, 2),
        (f64::from_bits(0x7ff8_0000_0000_0001), 2, 3),
    ]
}

fn hit(weight: f64, node: u128, edge: u128) -> NeighborHit {
    NeighborHit {
        node_id: Uuid::from_u128(node),
        edge_id: Uuid::from_u128(edge),
        relation: EdgeRelation::Extends,
        weight,
        name: None,
        kind: None,
        entity_type: None,
    }
}

#[test]
fn context_neighbors_order_raw_weights_and_node_edge_ties() {
    let expected: Vec<_> = ordered_cases()
        .into_iter()
        .map(|(weight, node, edge)| (hit(weight, node, edge), "outgoing"))
        .collect();
    let keys = |items: &[(NeighborHit, &str)]| {
        items
            .iter()
            .map(|(hit, direction)| {
                (
                    hit.weight.to_bits(),
                    hit.node_id,
                    hit.edge_id,
                    direction.to_string(),
                )
            })
            .collect::<Vec<_>>()
    };
    for rotation in 0..expected.len() {
        let mut actual = expected.clone();
        actual.rotate_left(rotation);
        actual.reverse();
        assert_eq!(
            compare_context_neighbors(&expected[9], &expected[0]),
            std::cmp::Ordering::Greater
        );
        actual.sort_by(compare_context_neighbors);
        assert_eq!(keys(&actual), keys(&expected), "rotation {rotation}");
    }
}

#[test]
fn hop2_neighbors_order_raw_weights_and_node_parent_ties() {
    let expected: Vec<_> = ordered_cases()
        .into_iter()
        .map(|(weight, node, parent)| {
            (
                Uuid::from_u128(parent),
                Uuid::from_u128(node),
                EdgeRelation::Extends,
                weight,
                "incoming",
            )
        })
        .collect();
    let keys = |items: &[(Uuid, Uuid, EdgeRelation, f64, &str)]| {
        items
            .iter()
            .map(|item| (item.0, item.1, item.3.to_bits()))
            .collect::<Vec<_>>()
    };
    for rotation in 0..expected.len() {
        let mut actual = expected.clone();
        actual.rotate_left(rotation);
        actual.reverse();
        assert_eq!(
            compare_hop2_neighbors(&expected[9], &expected[0]),
            std::cmp::Ordering::Greater
        );
        actual.sort_by(compare_hop2_neighbors);
        assert_eq!(keys(&actual), keys(&expected), "rotation {rotation}");
    }
}

#[test]
fn context_equal_keys_preserve_direction_order_and_nan_payloads() {
    for weights in [
        [0.5; 3],
        [
            f64::from_bits(0xfff8_0000_0000_0001),
            f64::NAN,
            f64::from_bits(0x7ff8_0000_0000_0002),
        ],
    ] {
        let mut actual: Vec<_> = weights
            .into_iter()
            .zip(["incoming", "outgoing", "both"])
            .map(|(weight, direction)| (hit(weight, 4, 5), direction))
            .collect();
        let expected: Vec<_> = actual
            .iter()
            .map(|(hit, direction)| (hit.weight.to_bits(), *direction))
            .collect();
        assert_eq!(
            compare_context_neighbors(&actual[0], &actual[1]),
            std::cmp::Ordering::Equal
        );
        actual.sort_by(compare_context_neighbors);
        assert_eq!(
            actual
                .iter()
                .map(|(hit, direction)| (hit.weight.to_bits(), *direction))
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn hop2_equal_keys_preserve_relation_direction_order_and_nan_payloads() {
    for weights in [
        [0.5; 3],
        [
            f64::from_bits(0xfff8_0000_0000_0001),
            f64::NAN,
            f64::from_bits(0x7ff8_0000_0000_0002),
        ],
    ] {
        let mut actual: Vec<_> = weights
            .into_iter()
            .zip([
                (EdgeRelation::Extends, "incoming"),
                (EdgeRelation::Contains, "outgoing"),
                (EdgeRelation::CompetesWith, "both"),
            ])
            .map(|(weight, (relation, direction))| {
                (
                    Uuid::from_u128(4),
                    Uuid::from_u128(5),
                    relation,
                    weight,
                    direction,
                )
            })
            .collect();
        let expected: Vec<_> = actual
            .iter()
            .map(|item| (item.2, item.3.to_bits(), item.4))
            .collect();
        assert_eq!(
            compare_hop2_neighbors(&actual[0], &actual[1]),
            std::cmp::Ordering::Equal
        );
        actual.sort_by(compare_hop2_neighbors);
        assert_eq!(
            actual
                .iter()
                .map(|item| (item.2, item.3.to_bits(), item.4))
                .collect::<Vec<_>>(),
            expected
        );
    }
}
