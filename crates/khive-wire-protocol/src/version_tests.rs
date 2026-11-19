use super::*;

#[test]
fn new_accepts_the_current_range() {
    let supported =
        SupportedVersions::new(ProtocolVersion::new(1), ProtocolVersion::new(1)).unwrap();
    assert_eq!(supported.min(), ProtocolVersion::new(1));
    assert_eq!(supported.max(), ProtocolVersion::new(1));
}

#[test]
fn new_rejects_a_range_above_current_version() {
    for (min, max) in [(1, 2), (2, 2)] {
        let err = SupportedVersions::new(ProtocolVersion::new(min), ProtocolVersion::new(max))
            .unwrap_err();
        assert_eq!(err, SupportedVersionsError::MaxAboveCurrent);
    }
}

#[test]
fn new_rejects_an_inverted_range() {
    let err = SupportedVersions::new(ProtocolVersion::new(5), ProtocolVersion::new(2)).unwrap_err();
    assert_eq!(err, SupportedVersionsError::InvertedRange);
}

#[test]
fn new_rejects_a_zero_min_version() {
    // Version 0 does not exist in this protocol; the floor is 1.
    // Checked before the ordering check, so a doubly-broken range
    // reports the zero floor first.
    let err = SupportedVersions::new(ProtocolVersion::new(0), ProtocolVersion::new(1)).unwrap_err();
    assert_eq!(err, SupportedVersionsError::MinVersionZero);
    let err = SupportedVersions::new(ProtocolVersion::new(0), ProtocolVersion::new(0)).unwrap_err();
    assert_eq!(err, SupportedVersionsError::MinVersionZero);
}

#[test]
fn current_version_is_at_least_one() {
    // The test mirror of the compile-time `const _: () = assert!` in
    // this module: the invariant gets an explicit, greppable failure
    // either way.
    assert!(CURRENT_VERSION.get() >= 1);
}

#[test]
fn contains_is_inclusive_at_both_bounds() {
    // Boundary inclusion: min and max themselves are supported; one
    // below min and one above max are not.
    let supported =
        SupportedVersions::new(ProtocolVersion::new(1), ProtocolVersion::new(1)).unwrap();
    assert!(!supported.contains(ProtocolVersion::new(0)));
    assert!(supported.contains(ProtocolVersion::new(1)));
    assert!(!supported.contains(ProtocolVersion::new(2)));
}

#[test]
fn current_saturates_at_version_one() {
    // The crate is at version 1, so there is no prior version and the
    // default range is [1, 1].
    let supported = SupportedVersions::current();
    assert_eq!(supported.min(), ProtocolVersion::new(1));
    assert_eq!(supported.max(), ProtocolVersion::new(1));
}
