use super::*;

struct MissingDependencyFactory {
    name: &'static str,
    requires: &'static [&'static str],
}

impl PackFactory for MissingDependencyFactory {
    fn name(&self) -> &'static str {
        self.name
    }

    fn requires(&self) -> &'static [&'static str] {
        self.requires
    }

    fn create(&self, _runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        panic!(
            "selection preflight must refuse before creating {}",
            self.name
        )
    }
}

static LEFT_FACTORY: MissingDependencyFactory = MissingDependencyFactory {
    name: "boot-preflight-aggregate-left",
    requires: &["boot-preflight-required-left"],
};
static RIGHT_FACTORY: MissingDependencyFactory = MissingDependencyFactory {
    name: "boot-preflight-aggregate-right",
    requires: &["boot-preflight-required-right"],
};
static EXTRA_FACTORY: MissingDependencyFactory = MissingDependencyFactory {
    name: "boot-preflight-aggregate-extra",
    requires: &["boot-preflight-required-extra"],
};

inventory::submit! { PackRegistration(&LEFT_FACTORY) }
inventory::submit! { PackRegistration(&RIGHT_FACTORY) }

const EXPECTED_DIAGNOSTIC: &str = concat!(
    "missing pack dependencies: ",
    "pack \"boot-preflight-aggregate-left\" requires \"boot-preflight-required-left\"; ",
    "pack \"boot-preflight-aggregate-right\" requires \"boot-preflight-required-right\"; ",
    "add all missing packs to the requested pack list"
);

fn names() -> Vec<String> {
    vec![
        LEFT_FACTORY.name.to_string(),
        RIGHT_FACTORY.name.to_string(),
    ]
}

fn assert_missing_pairs(error: PackLoadError, expected: &[(&str, &str)], diagnostic: &str) {
    let PackLoadError::MissingDependencies { ref missing } = error else {
        panic!("expected aggregate missing dependencies, got {error:?}");
    };
    let actual: Vec<_> = missing
        .iter()
        .map(|(pack, dep)| (pack.as_str(), dep.as_str()))
        .collect();
    assert_eq!(actual.as_slice(), expected);
    assert_eq!(error.to_string(), diagnostic);
}

fn assert_inventory_missing_pairs(error: PackLoadError) {
    assert_missing_pairs(
        error,
        &[
            (LEFT_FACTORY.name, LEFT_FACTORY.requires[0]),
            (RIGHT_FACTORY.name, RIGHT_FACTORY.requires[0]),
        ],
        EXPECTED_DIAGNOSTIC,
    );
}

#[test]
fn selection_preflight_reports_all_missing_dependency_pairs() {
    let error = PackRegistry::validate_pack_selection(&names())
        .expect_err("selection preflight must report both missing dependency pairs");
    assert_inventory_missing_pairs(error);
}

#[test]
fn single_runtime_registration_reports_all_missing_dependency_pairs_before_creation() {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    let error = PackRegistry::register_packs(&names(), runtime, &mut builder)
        .expect_err("single-runtime registration must report both missing dependency pairs");
    assert_inventory_missing_pairs(error);
}

#[test]
fn multi_runtime_registration_reports_all_missing_dependency_pairs_before_creation() {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    let error = PackRegistry::register_packs_with_runtimes(
        &names(),
        &HashMap::new(),
        &runtime,
        &mut builder,
    )
    .expect_err("multi-runtime registration must report both missing dependency pairs");
    assert_inventory_missing_pairs(error);
}

#[test]
fn extra_factory_registration_reports_all_missing_dependency_pairs_before_creation() {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    let error = PackRegistry::register_packs_with_runtimes_with_extra_factories(
        &[&EXTRA_FACTORY],
        &[
            LEFT_FACTORY.name.to_string(),
            EXTRA_FACTORY.name.to_string(),
        ],
        &HashMap::new(),
        &runtime,
        &mut builder,
    )
    .expect_err("extra-factory registration must include both missing dependency pairs");
    assert_missing_pairs(
        error,
        &[
            (LEFT_FACTORY.name, LEFT_FACTORY.requires[0]),
            (EXTRA_FACTORY.name, EXTRA_FACTORY.requires[0]),
        ],
        concat!(
            "missing pack dependencies: ",
            "pack \"boot-preflight-aggregate-left\" requires \"boot-preflight-required-left\"; ",
            "pack \"boot-preflight-aggregate-extra\" requires \"boot-preflight-required-extra\"; ",
            "add all missing packs to the requested pack list"
        ),
    );
}

#[test]
fn one_missing_dependency_preserves_legacy_error_contract() {
    let error = PackRegistry::validate_pack_selection(&[LEFT_FACTORY.name.to_string()])
        .expect_err("one missing dependency must preserve the singleton error");
    assert!(matches!(
        &error,
        PackLoadError::MissingDependency { pack, dep }
            if pack == LEFT_FACTORY.name && dep == LEFT_FACTORY.requires[0]
    ));
    assert_eq!(
        error.to_string(),
        concat!(
            "pack \"boot-preflight-aggregate-left\" requires \"boot-preflight-required-left\", ",
            "which is not in the requested pack list; ",
            "add --pack boot-preflight-required-left before --pack boot-preflight-aggregate-left"
        )
    );
}
