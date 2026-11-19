//! Preserve eager setup only when Criterion 0.5 selects its profiling mode.

pub fn is_profile_mode() -> bool {
    let mut bench = false;
    let mut profile = false;
    // Criterion validates options before calling the benchmark group. Without
    // --bench it selects test mode, even when --profile-time is present.
    for arg in std::env::args().skip(1).take_while(|arg| arg != "--") {
        if arg == "--test" || arg == "--list" {
            return false;
        }
        bench |= arg == "--bench";
        profile |= arg == "--profile-time" || arg.starts_with("--profile-time=");
    }
    bench && profile
}
