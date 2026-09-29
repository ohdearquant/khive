use std::time::Duration;

mod timing {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test_support/timing.rs"
    ));
}

pub fn assert_caller_latency(elapsed: Duration, checkout_timeout: Duration) {
    if let Some(bound) = timing::duration_bound(checkout_timeout * 10, None) {
        assert!(
            elapsed < bound,
            "writer checkout took {elapsed:?} with a {checkout_timeout:?} admission timeout; \
             timeout diagnostics must return within the {bound:?} caller bound"
        );
    }
}
