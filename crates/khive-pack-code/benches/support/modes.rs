pub fn reports_enabled() -> bool {
    let mut bench = false;
    for arg in std::env::args_os().skip(1).take_while(|arg| arg != "--") {
        if arg == "--test"
            || arg == "--profile-time"
            || arg
                .to_str()
                .is_some_and(|arg| arg.starts_with("--profile-time="))
        {
            return false;
        }
        bench |= arg == "--bench";
    }
    bench
}
