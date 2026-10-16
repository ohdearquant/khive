//! End-to-end acceptance test for the ADR-088 git-lifecycle pack (v0).
//!
//! Builds a synthetic fixture repo with `git` inside a tempdir, runs one
//! ingest pass against an in-memory runtime, and asserts the provenance
//! query genre works: traversing/searching from a pre-created `document`
//! entity via incoming `annotates` edges yields exactly the commits that
//! touched its path, and a squash-merge commit's PR edge resolves. Also
//! covers `KindHook` validation and secret-masking on ingested content.

#[path = "../src/test_process.rs"]
mod test_process;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use khive_pack_git::ingest::{run_ingest, IngestInclude, IngestOptions};
use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    AllowAllGate, BackendId, EmbedderProvider, EntityCreateSpec, KhiveRuntime, Namespace,
    NamespaceToken, RequestIdentity, RuntimeConfig, RuntimeError, RuntimeResult, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{
    types::{SqlStatement, SqlValue},
    Event,
};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use uuid::Uuid;

#[path = "support/digest_commit_resume.rs"]
mod digest_commit_resume;
#[path = "support/digest_commit_work.rs"]
mod digest_commit_work;
#[path = "support/digest_fetch_budget.rs"]
mod digest_fetch_budget;
#[path = "support/digest_resume.rs"]
mod digest_resume;
#[path = "support/digest_scale.rs"]
mod digest_scale;

include!("acceptance_tests/fixtures_tests.rs");

include!("acceptance_tests/provenance_tests.rs");

include!("acceptance_tests/commit_identity_tests.rs");

include!("acceptance_tests/replay_and_snapshot_tests.rs");

include!("acceptance_tests/changed_paths_tests.rs");

include!("acceptance_tests/path_filtering_tests.rs");

include!("acceptance_tests/content_masking_tests.rs");

include!("acceptance_tests/issue_metadata_masking_tests.rs");

include!("acceptance_tests/timestamp_validation_tests.rs");

include!("acceptance_tests/pull_request_masking_tests.rs");

include!("acceptance_tests/hook_validation_tests.rs");

include!("acceptance_tests/project_scoping_tests.rs");

include!("acceptance_tests/paging_floor_tests.rs");

include!("acceptance_tests/cursor_ordering_tests.rs");

include!("acceptance_tests/cursor_tie_tests.rs");

include!("acceptance_tests/digest_receipt_tests.rs");

include!("acceptance_tests/digest_budget_tests.rs");

include!("acceptance_tests/remote_source_status_tests.rs");

include!("acceptance_tests/cursor_refusal_tests.rs");

include!("acceptance_tests/commit_cursor_failure_tests.rs");

include!("acceptance_tests/remote_capability_tests.rs");

include!("acceptance_tests/digest_limit_tests.rs");

include!("acceptance_tests/embedding_capture_tests.rs");

include!("acceptance_tests/embedding_boundary_tests.rs");

include!("acceptance_tests/adr_entity_type_tests.rs");

include!("acceptance_tests/commit_metadata_masking_tests.rs");

include!("acceptance_tests/pull_request_metadata_tests.rs");
