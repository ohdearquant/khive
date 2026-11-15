//! Experimental raster ingest, exact visual retrieval, and pairwise preference learning over
//! Khive substrates.
//!
//! The pack is intentionally opt-in. Original bytes live in the configured
//! [`khive_storage::BlobStore`], graph identity lives in `visual_asset`
//! artifact entities, and immutable Lattice descriptor spaces live in named
//! Khive vector tables. In multi-backend configurations, entities use the
//! shared core graph while vector tables stay on the pack-selected runtime.
//! It does not register a text embedder provider.

pub mod handlers;
mod materialization;
mod model;
mod pack;
mod preference;
mod preference_artifact;
mod preference_handlers;
mod preprocess;
pub mod vocab;

use khive_runtime::KhiveRuntime;
use khive_types::{EntityTypeDef, HandlerDef, Pack};

use model::VisionModelState;

pub use preference_artifact::{
    legacy_preference_model_count, verify_legacy_preference_attachments,
    MoodboardLegacyPreferenceVerifier, VerifiedModelNetworkAttachment,
};

pub(crate) const PACK_NAME: &str = "moodboard";
pub(crate) const LATTICE_VERSION: &str = "0.15.0";

/// Opt-in Moodboard visual-retrieval and preference-learning pack.
pub struct MoodboardPack {
    runtime: KhiveRuntime,
    model: VisionModelState,
}

impl Pack for MoodboardPack {
    const NAME: &'static str = PACK_NAME;
    const NOTE_KINDS: &'static [&'static str] = vocab::NOTE_KINDS;
    const ENTITY_KINDS: &'static [&'static str] = vocab::ENTITY_KINDS;
    const HANDLERS: &'static [HandlerDef] = &vocab::MOODBOARD_HANDLERS;
    const ENTITY_TYPES: &'static [EntityTypeDef] = &vocab::MOODBOARD_ENTITY_TYPES;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl MoodboardPack {
    /// Bind a Moodboard pack to one runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self {
            runtime,
            model: VisionModelState::default(),
        }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }

    pub(crate) fn model_state(&self) -> &VisionModelState {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::LATTICE_VERSION;

    #[test]
    fn lattice_provenance_version_matches_all_exact_workspace_pins() {
        let Some(manifest) = read_workspace_file("Cargo.toml") else {
            return;
        };
        let Some(lock) = read_workspace_file("Cargo.lock") else {
            return;
        };
        let dependencies = manifest["workspace"]["dependencies"]
            .as_table()
            .expect("workspace.dependencies must be a table");
        let pack_manifest: toml::Value =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
                .expect("the pack manifest must be readable")
                .parse()
                .expect("the pack manifest must parse");
        let pack_dependencies = pack_manifest["dependencies"]
            .as_table()
            .expect("the pack manifest must have a dependencies table");

        // The descriptor records one lattice version, so every lattice crate the embedding,
        // inference and FANN code resolves to must be pinned to exactly that version.
        let closure = lattice_closure(
            &lock,
            &["lattice-embed", "lattice-inference", "lattice-fann"],
        );
        assert!(
            closure.len() > 3,
            "the lock must resolve the lattice roots to their dependencies: {closure:?}"
        );
        for dependency in &closure {
            let pin = match dependencies.get(dependency.as_str()) {
                Some(toml::Value::String(version)) => version.as_str(),
                Some(toml::Value::Table(specification)) => specification["version"]
                    .as_str()
                    .expect("lattice dependency table must contain a version"),
                Some(value) => panic!("unexpected {dependency} dependency shape: {value:?}"),
                None => panic!("{dependency} is resolved but not pinned in workspace.dependencies"),
            };
            assert_eq!(pin, format!("={LATTICE_VERSION}"), "{dependency}");
            assert!(
                pack_dependencies.contains_key(dependency.as_str()),
                "{dependency} is resolved but is not a dependency of this pack"
            );
        }
    }

    /// Reads a file from the workspace root, or `None` when it is absent, as it is in a
    /// published crate archive.
    fn read_workspace_file(name: &str) -> Option<toml::Value> {
        let path = format!("{}/../{name}", env!("CARGO_MANIFEST_DIR"));
        match std::fs::read_to_string(&path) {
            Ok(source) => {
                Some(source.parse().unwrap_or_else(|error| {
                    panic!("workspace {name} at {path} must parse: {error}")
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                println!(
                    "workspace {name} not found at {path} \
                     (published crate archive does not carry it); skipping the pin check"
                );
                None
            }
            Err(error) => panic!("workspace {name} at {path} is unreadable: {error}"),
        }
    }

    /// Every `lattice-*` package reachable from `roots` in the lockfile, the roots included.
    fn lattice_closure(lock: &toml::Value, roots: &[&str]) -> std::collections::BTreeSet<String> {
        let packages = lock["package"]
            .as_array()
            .expect("Cargo.lock must list its packages");
        let mut seen: std::collections::BTreeSet<String> =
            roots.iter().map(|root| (*root).to_string()).collect();
        let mut pending: Vec<String> = seen.iter().cloned().collect();
        while let Some(name) = pending.pop() {
            for package in packages
                .iter()
                .filter(|package| package["name"].as_str() == Some(name.as_str()))
            {
                for entry in package
                    .get("dependencies")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let entry = entry.as_str().expect("lock dependency entries are strings");
                    let dependency = entry.split(' ').next().unwrap_or(entry);
                    if dependency.starts_with("lattice-") && seen.insert(dependency.to_string()) {
                        pending.push(dependency.to_string());
                    }
                }
            }
        }
        seen
    }
}
