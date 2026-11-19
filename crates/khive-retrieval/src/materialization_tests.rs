use std::cell::{Cell, RefCell};
use std::future::ready;

use super::*;

include!("materialization_tests/admission.rs");

include!("materialization_tests/limits_and_input.rs");

include!("materialization_tests/loader_and_classifier.rs");

include!("materialization_tests/ranked_prefix.rs");

include!("materialization_tests/diagnostics.rs");

include!("materialization_tests/taxonomy_membership.rs");
