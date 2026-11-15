use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

use crate::WebPack;

// Standalone test runtimes do not carry a registry, so install kg+web's
// declared edge rules before the existing fixtures create linked records.
pub(crate) fn install_web_edge_rules(runtime: &KhiveRuntime) {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(WebPack::new(runtime.clone()));
    let registry = builder.build().expect("kg+web registry builds");
    runtime.install_edge_rules(registry.all_edge_rules());
}
