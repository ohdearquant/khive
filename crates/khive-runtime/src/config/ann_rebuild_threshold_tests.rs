use super::ann_rebuild_threshold_from_env;
use std::process::Command;

const CASE_KEY: &str = "KHIVE_THRESHOLD_HELPER_TEST_CASE";
const ENV_KEY: &str = "KHIVE_ANN_REBUILD_THRESHOLD";
const CHILD: &str = "config::ann_rebuild_threshold_tests::threshold_env_case_child";

fn run_child(case: &str, value: Option<&str>) {
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", CHILD, "--nocapture"])
        .env(CASE_KEY, case)
        .env_remove(ENV_KEY);
    if let Some(value) = value {
        command.env(ENV_KEY, value);
    }
    let output = command.output().expect("threshold child");
    assert!(
        output.status.success(),
        "case={case} value={value:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("THRESHOLD_CASE_COMPLETED"));
}

#[test]
fn threshold_unset_malformed_and_nonfinite_use_exact_default() {
    for value in [
        None,
        Some("invalid"),
        Some("NaN"),
        Some("inf"),
        Some("-inf"),
    ] {
        run_child("default", value);
    }
}

#[test]
fn threshold_rejects_zero_negative_and_above_one() {
    for value in ["0", "-0", "-0.1", "1.00001"] {
        run_child("default", Some(value));
    }
}

#[test]
fn threshold_accepts_positive_subnormal_and_closed_upper_boundary() {
    run_child("one", Some("1"));
    run_child("subnormal", Some("5e-324"));
    run_child("quarter", Some("0.25"));
}

#[test]
fn threshold_does_not_trim_environment_value() {
    for value in [" 0.25", "0.25 ", "\t0.25\n"] {
        run_child("default", Some(value));
    }
}

#[test]
fn threshold_samples_each_invocation_without_cache() {
    run_child("changes", Some("0.25"));
}

#[cfg(unix)]
#[test]
fn threshold_nonunicode_uses_exact_default() {
    use std::os::unix::ffi::OsStringExt;
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", CHILD, "--nocapture"])
        .env(CASE_KEY, "default")
        .env(ENV_KEY, std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .expect("non-Unicode child");
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("THRESHOLD_CASE_COMPLETED"));
}

#[test]
fn threshold_env_case_child() {
    let Ok(case) = std::env::var(CASE_KEY) else {
        return;
    };
    let actual = ann_rebuild_threshold_from_env();
    match case.as_str() {
        "default" => assert_eq!(actual.to_bits(), 0.20_f64.to_bits()),
        "one" => assert_eq!(actual.to_bits(), 1.0_f64.to_bits()),
        "quarter" => assert_eq!(actual.to_bits(), 0.25_f64.to_bits()),
        "subnormal" => assert_eq!(actual.to_bits(), f64::from_bits(1).to_bits()),
        "changes" => {
            assert_eq!(actual.to_bits(), 0.25_f64.to_bits());
            // This process runs only this exact test. No parent-process env is changed.
            std::env::set_var(ENV_KEY, "0.75");
            assert_eq!(
                ann_rebuild_threshold_from_env().to_bits(),
                0.75_f64.to_bits()
            );
            std::env::set_var(ENV_KEY, "invalid");
            assert_eq!(
                ann_rebuild_threshold_from_env().to_bits(),
                0.20_f64.to_bits()
            );
            std::env::remove_var(ENV_KEY);
            assert_eq!(
                ann_rebuild_threshold_from_env().to_bits(),
                0.20_f64.to_bits()
            );
        }
        other => panic!("unknown threshold case {other}"),
    }
    println!("THRESHOLD_CASE_COMPLETED {case}");
}
