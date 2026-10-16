use crate::{KhiveRuntime, VerbRegistry};
impl VerbRegistry {
    pub(crate) fn graph_lint_runtimes(&self, primary: &KhiveRuntime) -> Vec<KhiveRuntime> {
        match &self.kg_read_resolver {
            Some(resolver) => resolver.runtimes_for_lint(),
            None => vec![primary.clone()],
        }
    }
}
#[cfg(test)]
pub(crate) fn registry_with_lint_owners(
    primary: &KhiveRuntime,
    others: std::collections::HashMap<String, KhiveRuntime>,
) -> VerbRegistry {
    let mut registry = crate::VerbRegistryBuilder::new().build().unwrap();
    registry.kg_read_resolver = Some(std::sync::Arc::new(crate::kg_read::KgReadResolver::new(
        primary, &others,
    )));
    registry
}
