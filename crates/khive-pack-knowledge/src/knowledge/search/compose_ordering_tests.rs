use super::*;

fn ordered_cases() -> [(f32, &'static str); 11] {
    let tiny = 1.0e-5_f32;
    let next = f32::from_bits(tiny.to_bits() + 1);
    assert!(next > tiny && next - tiny < 2.0_f32.powi(-32));
    [
        (f32::INFINITY, "z-inf"),
        (0.25, "a-tie"),
        (0.25, "b-tie"),
        (next, "z-near"),
        (tiny, "a-near"),
        (0.0, "z-zero"),
        (-0.0, "a-zero"),
        (f32::NEG_INFINITY, "z-negative-inf"),
        (f32::from_bits(0xffc0_0001), "a-nan"),
        (f32::from_bits(0x7fc0_0002), "b-nan"),
        (f32::from_bits(0x7fc0_0001), "c-nan"),
    ]
}

fn equal_key_scores() -> [[f32; 3]; 2] {
    [
        [0.5; 3],
        [
            f32::from_bits(0xffc0_0001),
            f32::NAN,
            f32::from_bits(0x7fc0_0002),
        ],
    ]
}

fn text_item(score: f32, slug: &str, id: &str) -> ScoredTextItem {
    ScoredTextItem {
        id: id.to_string(),
        slug: slug.to_string(),
        name: id.to_string(),
        text: id.to_string(),
        score,
    }
}

fn entity(score: f32, id: &str, name: &str) -> KgEntityHit {
    KgEntityHit {
        id: id.to_string(),
        kind: "concept".to_string(),
        name: name.to_string(),
        description: name.to_string(),
        score,
    }
}

#[test]
fn compose_text_orders_raw_scores_by_slug() {
    let make = || {
        ordered_cases()
            .into_iter()
            .enumerate()
            .map(|(index, (score, slug))| text_item(score, slug, &format!("{:02}", 20 - index)))
            .collect::<Vec<_>>()
    };
    let keys = |items: &[ScoredTextItem]| {
        items
            .iter()
            .map(|item| {
                (
                    item.score.to_bits(),
                    item.slug.clone(),
                    item.id.clone(),
                    item.text.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let expected = make();
    assert_eq!(
        compare_scored_text_items(&expected[8], &expected[0]),
        std::cmp::Ordering::Greater
    );
    for rotation in 0..expected.len() {
        let mut actual = make();
        actual.rotate_left(rotation);
        actual.reverse();
        actual.sort_by(compare_scored_text_items);
        assert_eq!(keys(&actual), keys(&expected), "rotation {rotation}");
    }
}

#[test]
fn compose_text_equal_keys_keep_input_order_and_nan_payloads() {
    for scores in equal_key_scores() {
        let mut actual: Vec<_> = scores
            .into_iter()
            .zip(["z", "a", "m"])
            .map(|(score, id)| text_item(score, "same-slug", id))
            .collect();
        let keys = |items: &[ScoredTextItem]| {
            items
                .iter()
                .map(|item| (item.score.to_bits(), item.id.clone(), item.text.clone()))
                .collect::<Vec<_>>()
        };
        let expected = keys(&actual);
        assert_eq!(
            compare_scored_text_items(&actual[0], &actual[1]),
            std::cmp::Ordering::Equal
        );
        actual.sort_by(compare_scored_text_items);
        assert_eq!(keys(&actual), expected);
    }
}

#[test]
fn compose_entities_order_raw_scores_by_id() {
    let make = || {
        ordered_cases()
            .into_iter()
            .enumerate()
            .map(|(index, (score, id))| entity(score, id, &format!("{:02}", 20 - index)))
            .collect::<Vec<_>>()
    };
    let keys = |items: &[KgEntityHit]| {
        items
            .iter()
            .map(|item| {
                (
                    item.score.to_bits(),
                    item.id.clone(),
                    item.name.clone(),
                    item.description.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let expected = make();
    assert_eq!(
        compare_kg_entity_hits(&expected[8], &expected[0]),
        std::cmp::Ordering::Greater
    );
    for rotation in 0..expected.len() {
        let mut actual = make();
        actual.rotate_left(rotation);
        actual.reverse();
        actual.sort_by(compare_kg_entity_hits);
        assert_eq!(keys(&actual), keys(&expected), "rotation {rotation}");
    }
}

#[test]
fn compose_entities_equal_keys_keep_input_order_and_nan_payloads() {
    for scores in equal_key_scores() {
        let mut actual: Vec<_> = scores
            .into_iter()
            .zip(["z", "a", "m"])
            .map(|(score, name)| entity(score, "same-id", name))
            .collect();
        let keys = |items: &[KgEntityHit]| {
            items
                .iter()
                .map(|item| {
                    (
                        item.score.to_bits(),
                        item.name.clone(),
                        item.description.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let expected = keys(&actual);
        assert_eq!(
            compare_kg_entity_hits(&actual[0], &actual[1]),
            std::cmp::Ordering::Equal
        );
        actual.sort_by(compare_kg_entity_hits);
        assert_eq!(keys(&actual), expected);
    }
}
