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
