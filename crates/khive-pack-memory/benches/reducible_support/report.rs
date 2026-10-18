//! Common custom-harness output; no runtime or fixture is constructed here.
use serde_json::json;

pub fn list_requested(ids: &[&str]) -> bool {
    if std::env::args().any(|arg| arg == "--list") {
        for id in ids {
            println!("{id}: benchmark");
        }
        return true;
    }
    false
}

pub fn regression_case() -> Option<String> {
    std::env::args().find_map(|arg| arg.strip_prefix("--regression-case=").map(str::to_owned))
}

pub fn assert_not_list_probe() {
    assert_ne!(
        regression_case().as_deref(),
        Some("list-only"),
        "--list reached workload boundary"
    );
}

pub fn emit(id: &str, statistic: &str, value: f64, n: usize) {
    assert!(value.is_finite() && value >= 0.0 && n > 0);
    println!(
        "BENCH-JSON {}",
        json!({"id": id, "statistic": statistic, "value": value, "n": n})
    );
}
