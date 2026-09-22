//! ADR-191 D6 / ADR-192 S4 acceptance: a pack factory the caller supplies
//! directly — never `inventory::submit!`-registered, so it has no presence in
//! kkernel's own linked set — is discovered by name through
//! `kkernel::compose::compose_registry_with_extra_packs` and its verb
//! dispatches. Control: the same name, with no extra factory supplied, is
//! not in this test binary's linked set either, so composition refuses it as
//! `UnknownPack`.

use std::collections::HashMap;

use async_trait::async_trait;
use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, PackFactory, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, VerbCategory, Visibility};
use kkernel::compose::compose_registry_with_extra_packs;
use serde_json::{json, Value};

static ECHOTEST_HANDLERS: [HandlerDef; 1] = [HandlerDef {
    name: "echotest.ping",
    description:
        "Test-only extra-factory pack (never inventory-registered): returns {\"pong\": true}.",
    visibility: Visibility::Verb,
    category: VerbCategory::Directive,
    params: &[],
}];

struct EchoTestPack {
    #[allow(dead_code)]
    runtime: KhiveRuntime,
}

#[async_trait]
impl PackRuntime for EchoTestPack {
    fn name(&self) -> &str {
        "echotest"
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &ECHOTEST_HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "echotest.ping" => Ok(json!({ "pong": true })),
            _ => Err(RuntimeError::InvalidInput(format!(
                "echotest pack does not handle verb {verb:?}"
            ))),
        }
    }
}

struct EchoTestPackFactory;

impl PackFactory for EchoTestPackFactory {
    fn name(&self) -> &'static str {
        "echotest"
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(EchoTestPack { runtime })
    }
}

static ECHOTEST_FACTORY: EchoTestPackFactory = EchoTestPackFactory;

#[tokio::test]
async fn extra_pack_factory_is_discovered_by_name_and_dispatches() {
    let default_runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let names = vec!["echotest".to_string()];
    let runtimes: HashMap<String, KhiveRuntime> = HashMap::new();
    let extra: [&'static dyn PackFactory; 1] = [&ECHOTEST_FACTORY];

    let registry = compose_registry_with_extra_packs(&names, &runtimes, &default_runtime, &extra)
        .expect("registry composes with the extra factory supplied explicitly");

    let reply = registry
        .dispatch("echotest.ping", json!({}))
        .await
        .expect("the extra pack's verb dispatches through the composed registry");
    assert_eq!(reply["pong"], true);
}

#[tokio::test]
async fn control_without_extra_factory_refuses_as_unknown_pack() {
    let default_runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let names = vec!["echotest".to_string()];
    let runtimes: HashMap<String, KhiveRuntime> = HashMap::new();

    let err = match compose_registry_with_extra_packs(&names, &runtimes, &default_runtime, &[]) {
        Ok(_) => panic!("without the extra factory, echotest is not in this binary's linked set"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("UnknownPack") && message.contains("echotest"),
        "expected an UnknownPack-shaped refusal, got: {message}"
    );
}
