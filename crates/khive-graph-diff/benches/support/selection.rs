pub struct Selection {
    filter: Option<String>,
    list: bool,
    smoke: bool,
    help: bool,
}

impl Selection {
    pub fn from_args() -> Self {
        let mut filter = None;
        let mut list = false;
        let mut bench = false;
        let mut test = false;
        let mut help = false;
        for arg in std::env::args().skip(1) {
            match arg.as_str() {
                "--bench" => bench = true,
                "--test" => test = true,
                "--quick" | "--noplot" => {}
                "--list" => list = true,
                "--help" | "-h" => help = true,
                _ if !arg.starts_with('-') && filter.is_none() => filter = Some(arg),
                _ => panic!("unsupported report argument; see --help"),
            }
        }
        if help {
            println!("[--bench] [--test] [--quick] [--noplot] [--list] [SUBSTRING]: fixed reports; --bench runs every size, --test or no --bench runs 1k smoke only; SUBSTRING is literal");
        }
        Self {
            filter,
            list,
            smoke: test || !bench,
            help,
        }
    }

    pub fn selected(&self, name: &str, count: usize) -> bool {
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
        if self.smoke && count > 1_000 {
            println!(
                "{}",
                serde_json::json!({
                    "scenario": name,
                    "status": "UNMEASURED",
                    "reason": "test/default smoke executes only 1000-record scenes; use --bench for every size"
                })
            );
            return false;
        }
        true
    }
}
