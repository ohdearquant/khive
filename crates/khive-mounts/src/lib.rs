//! Operator-configured, pinned stdio tool sources on the ordinary verb path.

mod catalog;
mod client;
mod error;
mod store;

use std::{collections::BTreeSet, sync::RwLock};

use async_trait::async_trait;
use khive_runtime::{
    mount_config::MountConfig, mounted_verb::MountedVerb, KhiveRuntime, NamespaceToken,
    PackRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::Event;
use khive_types::{EventKind, HandlerDef, SubstrateKind};
use serde_json::{json, Value};

use client::Client;
use error::Failure;

/// One runtime-owned process with an operator-pinned, persisted catalog.
pub struct MountedPack {
    config: MountConfig,
    runtime: KhiveRuntime,
    client: Client,
    snapshot: RwLock<(i64, Vec<MountedVerb>)>,
}
impl MountedPack {
    /// Start and discover a source. An existing pin is never silently replaced.
    pub async fn start(config: MountConfig, runtime: KhiveRuntime) -> Result<Self, RuntimeError> {
        khive_runtime::mount_config::validate_mounts(std::slice::from_ref(&config))
            .map_err(|_| Failure::error("invalid_configuration").wire(&config.name))?;
        if khive_runtime::PackRegistry::discovered_names().contains(&config.name.as_str()) {
            return Err(Failure::error("namespace_collision").wire(&config.name));
        }
        let client = Client::start(config.clone())
            .await
            .map_err(|error| error.wire(&config.name))?;
        let published = client
            .catalog()
            .await
            .map_err(|error| error.wire(&config.name))?;
        let tools =
            catalog::pin(&config, &published, 1).map_err(|error| error.wire(&config.name))?;
        store::initialize(&runtime.sql(), &config.name, &tools).await?;
        let persisted = store::load(&runtime.sql(), &config.name)
            .await?
            .ok_or_else(|| Failure::error("catalog_unavailable").wire(&config.name))?;
        Ok(Self {
            config,
            runtime,
            client,
            snapshot: RwLock::new(persisted),
        })
    }

    fn refresh_snapshot(&self, generation: i64, tools: Vec<MountedVerb>) {
        let mut snapshot = self
            .snapshot
            .write()
            .expect("mounted catalog snapshot lock");
        // Concurrent refreshes must not replace a newer local generation with an older read.
        if generation >= snapshot.0 {
            *snapshot = (generation, tools);
        }
    }

    /// Operator-only re-pin: discovery precedes one catalog CAS and its audit.
    pub async fn repin(&self, operator: &str) -> Result<Value, RuntimeError> {
        let (generation, old) = store::load(&self.runtime.sql(), &self.config.name)
            .await?
            .ok_or_else(|| Failure::error("catalog_unavailable").wire(&self.config.name))?;
        let next = generation
            .checked_add(1)
            .ok_or_else(|| Failure::error("generation_exhausted").wire(&self.config.name))?;
        let published = self
            .client
            .catalog()
            .await
            .map_err(|error| error.wire(&self.config.name))?;
        let tools = catalog::pin(&self.config, &published, next)
            .map_err(|error| error.wire(&self.config.name))?;
        let mut result = catalog::diff(&old, &tools);
        result["mount"] = json!(self.config.name);
        result["generation"] = json!(next);
        result["operator"] = json!(operator);
        let audit = Event::new(
            self.runtime.config().default_namespace.as_str(),
            "mount.repin",
            EventKind::Audit,
            SubstrateKind::Event,
            operator,
        )
        .with_payload(result.clone());
        store::replace(
            &self.runtime.sql(),
            &self.config.name,
            generation,
            &tools,
            &audit,
        )
        .await?;
        self.refresh_snapshot(next, tools);
        Ok(result)
    }
}

#[async_trait]
impl PackRuntime for MountedPack {
    fn name(&self) -> &str {
        &self.config.name
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &[]
    }
    fn mounted_namespace(&self) -> Option<&str> {
        Some(&self.config.name)
    }
    fn mounted_catalog_snapshot(&self) -> Vec<MountedVerb> {
        self.snapshot
            .read()
            .expect("mounted catalog snapshot lock")
            .1
            .clone()
    }
    async fn mounted_catalog(&self) -> Result<Vec<MountedVerb>, RuntimeError> {
        let (generation, tools) = store::load(&self.runtime.sql(), &self.config.name)
            .await.ok().flatten().ok_or_else(|| {
                tracing::warn!(mount = %self.config.name, reason = "catalog_drift", "mounted catalog unavailable");
                Failure::error("catalog_drift").wire(&self.config.name)
            })?;
        self.refresh_snapshot(generation, tools.clone());
        Ok(tools)
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::UnknownVerb(verb.into()))
    }
    async fn dispatch_mounted(
        &self,
        definition: &MountedVerb,
        _verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.client.invoke(definition.clone(), params, self.runtime.sql()).await.map_err(|error| {
            tracing::warn!(mount = %self.config.name, class = error.class, reason = error.reason, "mounted call refused");
            error.wire(&self.config.name)
        })
    }
}

/// Boot each configured source independently; failures do not enable its routes.
pub async fn start_mounts(runtime: &KhiveRuntime) -> Vec<MountedPack> {
    let reserved: BTreeSet<_> = khive_runtime::PackRegistry::discovered_names()
        .into_iter()
        .collect();
    let mut names = BTreeSet::new();
    let mut packs = Vec::new();
    for config in &runtime.config().mounts {
        if reserved.contains(config.name.as_str()) || !names.insert(config.name.clone()) {
            tracing::warn!(mount = %config.name, reason = "namespace_collision", "mount registration refused");
            continue;
        }
        match MountedPack::start(config.clone(), runtime.clone()).await {
            Ok(pack) => packs.push(pack),
            Err(error) => {
                tracing::warn!(mount = %config.name, %error, "mount registration refused")
            }
        }
    }
    packs
}

/// Add operator-configured sources to a native registry at the async boot seam.
pub async fn register_mounts(
    runtime: &KhiveRuntime,
    builder: &mut VerbRegistryBuilder,
) -> Result<(), RuntimeError> {
    for pack in start_mounts(runtime).await {
        builder.register_mounted(Box::new(pack))?;
    }
    Ok(())
}
