use std::sync::atomic::{AtomicUsize, Ordering};

use khive_storage::read_env_number;

static NEXT_KEY: AtomicUsize = AtomicUsize::new(0);

struct Setting(String);

impl Setting {
    fn new() -> Self {
        Self(format!(
            "KHIVE_NUMERIC_ENV_TEST_{}_{}",
            std::process::id(),
            NEXT_KEY.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn set(&self, value: Option<&str>) {
        match value {
            Some(value) => std::env::set_var(&self.0, value),
            None => std::env::remove_var(&self.0),
        }
    }
}

impl Drop for Setting {
    fn drop(&mut self) {
        std::env::remove_var(&self.0);
    }
}

#[test]
fn numeric_env_keeps_trimmed_parse_types_and_absence() {
    let setting = Setting::new();
    for value in [None, Some(""), Some("abc"), Some("-1"), Some("256")] {
        setting.set(value);
        assert_eq!(read_env_number::<u8>(&setting.0), None, "{value:?}");
    }
    setting.set(Some(" \t255\n"));
    assert_eq!(read_env_number::<u8>(&setting.0), Some(255));
    setting.set(Some(" -1 "));
    assert_eq!(read_env_number::<i64>(&setting.0), Some(-1));
    setting.set(Some("0"));
    assert_eq!(read_env_number::<usize>(&setting.0), Some(0));
}

#[cfg(unix)]
#[test]
fn numeric_env_refuses_non_unicode_values() {
    use std::os::unix::ffi::OsStringExt;
    let setting = Setting::new();
    std::env::set_var(&setting.0, std::ffi::OsString::from_vec(vec![0xff]));
    assert_eq!(read_env_number::<u64>(&setting.0), None);
}
