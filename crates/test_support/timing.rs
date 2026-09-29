pub fn duration_bound(
    tight: std::time::Duration,
    instrumented: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    let profile_file = std::env::var_os("LLVM_PROFILE_FILE");
    select_duration_bound(tight, instrumented, profile_file.as_deref())
}

pub fn select_duration_bound(
    tight: std::time::Duration,
    instrumented: Option<std::time::Duration>,
    profile_file: Option<&std::ffi::OsStr>,
) -> Option<std::time::Duration> {
    if profile_file.is_some() {
        instrumented
    } else {
        Some(tight)
    }
}
