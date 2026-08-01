use std::env;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};

#[path = "src/build_info_support.rs"]
mod build_info_support;

use build_info_support::{git_output, source_revision};

const UNSTAMPED_REVISION: &str = "unstamped";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/build_info.rs");
    println!("cargo:rerun-if-changed=src/build_info_support.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let repo_root = git_output(&manifest_dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);

    if let Some(repo_root) = repo_root.as_deref() {
        register_git_inputs(repo_root);
        register_tracked_files(repo_root);
    }

    let revision = repo_root
        .as_deref()
        .and_then(source_revision)
        .unwrap_or_else(|| UNSTAMPED_REVISION.to_string());
    let build_time = build_time();

    println!("cargo:rustc-env=KHIVE_SOURCE_REVISION={revision}");
    println!("cargo:rustc-env=KHIVE_BUILD_TIME={build_time}");
}

fn build_time() -> String {
    let timestamp = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|raw| raw.parse::<i64>().ok())
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .unwrap_or_else(Utc::now);
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn register_git_inputs(repo_root: &Path) {
    let mut inputs = vec![
        "HEAD".to_string(),
        "index".to_string(),
        "packed-refs".to_string(),
    ];
    if let Some(reference) = git_output(repo_root, &["symbolic-ref", "-q", "HEAD"]) {
        inputs.push(reference);
    }

    for input in inputs {
        let Some(path) = git_output(repo_root, &["rev-parse", "--git-path", &input]) else {
            continue;
        };
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            repo_root.join(path)
        };
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn register_tracked_files(repo_root: &Path) {
    let Some(files) = git_output(repo_root, &["ls-files"]) else {
        return;
    };
    for file in files.lines().filter(|line| !line.is_empty()) {
        println!("cargo:rerun-if-changed={}", repo_root.join(file).display());
    }
}
