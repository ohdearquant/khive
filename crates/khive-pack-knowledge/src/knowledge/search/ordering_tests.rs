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

fn hit(score: f32, slug: &str, id: &str) -> ScoredHit {
    ScoredHit {
        id: id.to_string(),
        slug: slug.to_string(),
        name: id.to_string(),
        content: None,
        tags: None,
        atom_embed_text: None,
        finalized: false,
        is_domain: false,
        status: None,
        score,
        provenance: ScoreProvenance::lexical(),
    }
}

#[test]
fn search_and_suggest_order_raw_scores_by_slug() {
    let expected: Vec<_> = ordered_cases()
        .into_iter()
        .enumerate()
        .map(|(index, (score, slug))| hit(score, slug, &format!("{:02}", 20 - index)))
        .collect();
    let keys = |hits: &[ScoredHit]| {
        hits.iter()
            .map(|hit| {
                (
                    hit.score.to_bits(),
                    hit.slug.clone(),
                    hit.id.clone(),
                    hit.provenance,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        compare_scored_hits(&expected[8], &expected[0]),
        std::cmp::Ordering::Greater
    );
    for rotation in 0..expected.len() {
        let mut actual = expected.clone();
        actual.rotate_left(rotation);
        actual.reverse();
        actual.sort_by(compare_scored_hits);
        assert_eq!(keys(&actual), keys(&expected), "rotation {rotation}");
    }
}

#[test]
fn search_equal_keys_keep_ids_provenance_and_nan_payload_order() {
    for scores in equal_key_scores() {
        let mut actual: Vec<_> = scores
            .into_iter()
            .zip(["z", "a", "m"])
            .map(|(score, id)| hit(score, "same-slug", id))
            .collect();
        actual[1].provenance = ScoreProvenance::ann();
        let keys = |hits: &[ScoredHit]| {
            hits.iter()
                .map(|hit| (hit.score.to_bits(), hit.id.clone(), hit.provenance))
                .collect::<Vec<_>>()
        };
        let expected = keys(&actual);
        assert_eq!(
            compare_scored_hits(&actual[0], &actual[1]),
            std::cmp::Ordering::Equal
        );
        actual.sort_by(compare_scored_hits);
        assert_eq!(keys(&actual), expected);
    }
}
