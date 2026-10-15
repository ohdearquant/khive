use super::*;

#[test]
fn crosses_floor_is_write_size_aware_at_the_exact_boundary() {
    // Exact-boundary case, verbatim from the report: `available ==
    // floor_bytes + 1` must still refuse a 2-byte write. A floor-only
    // check (`available < floor_bytes`) would NOT catch this — 101 is
    // not below 100 — but the write's own size must be subtracted first.
    assert!(crosses_floor(101, 2, 100));
    assert!(!crosses_floor(101, 1, 100));
}

#[test]
fn crosses_floor_accepts_a_write_that_lands_exactly_on_the_floor() {
    assert!(!crosses_floor(100, 0, 100));
}

#[test]
fn crosses_floor_rejects_a_write_that_lands_one_byte_under_the_floor() {
    assert!(crosses_floor(100, 1, 100));
}

#[test]
fn crosses_floor_saturates_instead_of_underflowing_when_write_exceeds_available() {
    assert!(crosses_floor(10, 100, 50));
    // floor_bytes == 0 means "no floor enforced" (the convention every
    // other test in this file uses via `store(0)`) — even a write far
    // exceeding available space is not refused by the floor check itself
    // in that case; `saturating_sub` floors the subtraction at 0, and
    // `0 < 0` is false.
    assert!(!crosses_floor(10, 100, 0));
}

// ── Default floor scales with the volume (ADR-111 Amendment 6) ───────────────

const GIB: u64 = 1024 * 1024 * 1024;

#[test]
fn default_floor_is_two_percent_of_a_large_volume() {
    assert_eq!(
        FsBlobStore::default_floor_for_capacity(1_000_000_000_000),
        20_000_000_000
    );
    assert_eq!(
        FsBlobStore::default_floor_for_capacity(4096 * GIB),
        4096 * GIB / 50
    );
}

#[test]
fn default_floor_never_drops_below_five_gib_on_a_small_volume() {
    assert_eq!(FsBlobStore::MIN_DEFAULT_FLOOR_BYTES, 5 * GIB);
    for capacity in [0, 64 * GIB, 100 * GIB, 250 * GIB] {
        assert_eq!(
            FsBlobStore::default_floor_for_capacity(capacity),
            5 * GIB,
            "capacity {capacity}"
        );
    }
    // The first volume size whose 2% exceeds the minimum.
    assert_eq!(
        FsBlobStore::default_floor_for_capacity(250 * GIB + 50),
        5 * GIB + 1
    );
}

#[test]
fn resolved_floor_keeps_a_configured_value_including_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(FsBlobStore::resolve_floor(dir.path(), Some(0)), 0);
    assert_eq!(
        FsBlobStore::resolve_floor(dir.path(), Some(100_000_000_000)),
        100_000_000_000
    );
}

#[test]
fn resolved_floor_without_configuration_follows_the_volume_of_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let capacity = fs4::total_space(dir.path()).unwrap();
    let expected = FsBlobStore::default_floor_for_capacity(capacity);
    assert_eq!(FsBlobStore::resolve_floor(dir.path(), None), expected);
    // A root that does not exist yet is measured on its parent's volume.
    assert_eq!(
        FsBlobStore::resolve_floor(&dir.path().join("blobs-not-yet-created"), None),
        expected
    );
}
