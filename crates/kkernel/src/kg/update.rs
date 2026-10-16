//! Update a schema remote's Git commit pin without fetching an archive.

use anyhow::Result;
use khive_vcs::remote_pin::update_remote_pin;
use khive_vcs::sync::RemoteName;

use super::types::UpdateArgs;

pub(super) fn cmd_update(args: UpdateArgs) -> Result<()> {
    let remote = RemoteName::parse(args.remote)?;
    let report = update_remote_pin(&args.repo, &remote, args.git_ref.as_deref())?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
