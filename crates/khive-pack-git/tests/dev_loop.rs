//! Local git contract tests. The CAS control removes update-ref's expected old
//! value; the at-ref-move test must then fail by observing the rival overwritten.
//! The hooks control removes core.hooksPath=/dev/null; the hook marker test must
//! then fail. These controls require separate source mutation runs.

#![cfg(unix)]

#[path = "../src/test_process.rs"]
mod test_process;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use khive_pack_blob::BlobPack;
use khive_pack_exec::ExecPack;
use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::blob_tree as tree;
use khive_runtime::engine_config::{
    GitWriteActorConfig, GitWriteEntryConfig, GitWriteSectionConfig,
};
use khive_runtime::{
    KhiveRuntime, RequestIdentity, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{ContentRef, MAX_BLOB_WHOLE_BYTES};
use serde_json::{json, Value};

include!("dev_loop_tests/fixtures_tests.rs");

include!("dev_loop_tests/repository_guard_tests.rs");

include!("dev_loop_tests/manifest_commit_tests.rs");

include!("dev_loop_tests/policy_and_hardening_tests.rs");

include!("dev_loop_tests/update_ref_tests.rs");

include!("dev_loop_tests/receipt_paging_tests.rs");

include!("dev_loop_tests/checkout_and_diff_tests.rs");

include!("dev_loop_tests/recovery_tests.rs");

include!("dev_loop_tests/status_tests.rs");

include!("dev_loop_tests/log_tests.rs");

include!("dev_loop_tests/repository_safety_tests.rs");
