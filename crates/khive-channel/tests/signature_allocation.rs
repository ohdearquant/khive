//! Deterministic count gate in its own test binary; timing is never asserted.
use std::hint::black_box;

#[path = "support/allocation_counter.rs"]
mod allocation_counter;
#[path = "../benches/support/signed_fixture.rs"]
mod signed_fixture;

#[test]
fn signature_verification_does_not_allocate() {
    let fixture = signed_fixture::SignatureFixture::new();
    // No verifier warm-up: every one of these 1000 calls is observed.
    let (all_valid, allocations) = allocation_counter::measure(|| {
        let mut all_valid = true;
        for _ in 0..1000 {
            all_valid &= fixture
                .key
                .verify(black_box(&fixture.input), black_box(&fixture.signature))
                .is_ok();
        }
        black_box(all_valid)
    });
    assert!(
        all_valid,
        "generated signatures must pass actual verification"
    );
    assert_eq!(allocations, 0, "1000 prepared verifies must not allocate");
}

#[test]
fn allocation_observer_detects_heap_growth() {
    let (allocation, observed) =
        allocation_counter::measure(|| black_box(Box::new(black_box([42_u8; 64]))));
    assert!(observed > 0, "observer must detect a real heap allocation");
    assert_eq!(allocation[0], 42);
    drop(allocation);
}
