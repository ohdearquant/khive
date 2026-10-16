use super::*;
use khive_storage::note::Note;
use serde_json::json;

include!("message_tests/atomic_rollback_tests.rs");

include!("message_tests/thread_and_vector_tests.rs");

include!("message_tests/outcomes_and_provenance_tests.rs");

include!("message_tests/projection_tests.rs");
