//! Typed `gh <kind> list --json` page fetch shared by the pull request and
//! issue ingest paths.

use std::path::Path;

use anyhow::{Context, Result};

use super::{gh_json, search_query};

pub(super) async fn fetch_page<T: serde::de::DeserializeOwned>(
    repo: &Path,
    gh_repo: &str,
    floor: Option<&str>,
    limit: usize,
    kind: &str,
    fields: &str,
    parse_context: &'static str,
) -> Result<Vec<T>> {
    let search = search_query(floor);
    let limit = limit.to_string();
    let raw = gh_json(
        repo,
        gh_repo,
        &[
            kind,
            "list",
            "--state",
            "all",
            "--search",
            search.as_str(),
            "--limit",
            &limit,
            "--json",
            fields,
        ],
    )
    .await?;
    serde_json::from_str(&raw).context(parse_context)
}
