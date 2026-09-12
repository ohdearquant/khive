macro_rules! sql {
    ($name:literal) => {
        const {
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sql/", $name, ".sql"))
                .trim_ascii_end()
        }
    };
}
pub(crate) use sql;
