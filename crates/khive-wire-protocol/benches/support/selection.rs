pub struct Selection {
    filter: Option<String>,
    list: bool,
    help: bool,
}
impl Selection {
    pub fn from_args() -> Self {
        let mut result = Self {
            filter: None,
            list: false,
            help: false,
        };
        for arg in std::env::args().skip(1) {
            match arg.as_str() {
                "--bench" | "--test" | "--quick" | "--noplot" => {}
                "--list" => result.list = true,
                "--help" | "-h" => result.help = true,
                _ if !arg.starts_with('-') && result.filter.is_none() => result.filter = Some(arg),
                _ => panic!("unsupported allocation-report argument; see --help"),
            }
        }
        if result.help {
            println!("[--bench] [--test] [--quick] [--noplot] [--list] [SUBSTRING]: each selected allocation scene runs once at every size; SUBSTRING is literal, no latency estimates");
        }
        result
    }
    pub fn selected(&self, name: &str) -> bool {
        if self.help
            || self
                .filter
                .as_ref()
                .is_some_and(|filter| !name.contains(filter))
        {
            return false;
        }
        if self.list {
            println!("{name}: benchmark");
            return false;
        }
        true
    }
}
