//! `kkernel kg fetch` — fetch a remote KG archive.

use anyhow::{Context, Result};

use super::types::FetchArgs;
use crate::sync::RemoteName;

pub(super) async fn cmd_fetch(args: FetchArgs) -> Result<()> {
    let name = RemoteName::parse(args.remote).context("invalid --remote")?;

    let pin = args
        .pin
        .as_deref()
        .map(khive_vcs::SnapshotId::from_prefixed)
        .transpose()
        .context("invalid --pin")?;

    let remote = crate::sync::RemoteConfig {
        name,
        url: args.url,
        git_ref: args.git_ref,
        namespace: args.namespace,
        pin,
    };

    let report = crate::sync::run_sync_remote(&args.repo, &remote, args.repin)
        .await
        .with_context(|| format!("fetch remote {:?}", remote.name))?;
    let json = serde_json::to_string(&report).expect("serialize RemoteSyncReport");
    println!("{json}");
    Ok(())
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
