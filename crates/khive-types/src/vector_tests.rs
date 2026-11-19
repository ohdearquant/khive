use super::*;

#[test]
fn visited_set_wraparound_resets_marks() {
    let mut vs = VisitedSet {
        marks: vec![0; 4],
        generation: u64::MAX,
    };
    vs.mark_if_new(0);
    vs.clear();
    assert!(vs.mark_if_new(0));
    assert!(!vs.mark_if_new(0));
}

#[test]
fn default_is_cosine() {
    assert_eq!(DistanceMetric::default(), DistanceMetric::Cosine);
}
