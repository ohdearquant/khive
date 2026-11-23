//! `git.pr_edit` and `git.pr_close` (ADR-182 Amendment 17): change a pull request's title or
//! body, or close it, under the same gates as the other pull request writes.
//!
//! Both verbs reach this module from `perform_remote` after the shared checks: allowlist, caller
//! policy, credential, platform account, repository identity, the pull request read and the
//! `expected_head` comparison. What is decided here is who may change a pull request: the
//! account or actor that opened it, or a caller the `<verb>.others` policy allows.

use khive_runtime::engine_config::GitWriteRepositoryConfig;
use khive_runtime::{NamespaceToken, VerbRegistry};
use serde_json::{json, Map, Value};

use super::{endpoint, number, observed_sha, observed_string};
use crate::local_handlers::{checked_policy, optional, required, Failure};
use crate::receipts::{self, Receipt};
use crate::GitPack;

/// What `perform_remote` has already established about the pull request and the caller.
pub(super) struct Opened<'a> {
    pub(super) secret: &'a str,
    pub(super) target: &'a GitWriteRepositoryConfig,
    pub(super) pr: &'a Value,
    pub(super) login: &'a str,
    pub(super) credential_ref: &'a str,
}

/// The PATCH body for `git.pr_edit`: the fields the caller supplied, as supplied.
///
/// The reply must carry each field as sent, so a title the platform could store altered
/// (surrounding whitespace, a line break or another control character) is refused here: its
/// edit could land and still leave the receipt unknown.
fn edit_fields(params: &Value) -> Result<Map<String, Value>, Failure> {
    let mut fields = Map::new();
    if let Some(title) = optional(params, "title")? {
        if title.trim() != title || title.chars().any(char::is_control) {
            return Err(Failure::invalid(
                "title must not start or end with whitespace or hold a control character",
            ));
        }
        fields.insert("title".into(), json!(title));
    }
    if let Some(body) = params.get("body") {
        let body = body
            .as_str()
            .ok_or_else(|| Failure::invalid("body must be a string"))?;
        fields.insert("body".into(), json!(body));
    }
    if fields.is_empty() {
        return Err(Failure::invalid("git.pr_edit needs title, body or both"));
    }
    Ok(fields)
}

/// Argument rules the schema cannot state alone; called from `validate`.
pub(super) fn validate(verb: &str, params: &Value) -> Result<(), Failure> {
    number(params)?;
    if verb == "git.pr_edit" {
        edit_fields(params)?;
    }
    Ok(())
}

impl GitPack {
    /// Who may change this pull request: its opener by platform login, its opener by a
    /// `git.pr_open` receipt of this actor or credential, or a caller the `<verb>.others`
    /// policy allows. The answer is named on the result.
    async fn pr_change_authority(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        verb: &str,
        receipt: &mut Receipt,
        opened: &Opened<'_>,
    ) -> Result<Value, Failure> {
        let n = number(&receipt.inputs)?;
        if opened
            .login
            .eq_ignore_ascii_case(observed_string(opened.pr, "/user/login")?)
        {
            return Ok(json!({"source":"platform_login"}));
        }
        if self
            .opened_by_actor_or_reference(receipt, n, opened.credential_ref)
            .await?
        {
            return Ok(json!({"source":"pr_open_receipt"}));
        }
        let policy = checked_policy(registry, token, &format!("{verb}.others"))
            .await
            .unwrap_or_else(
                |_| json!({"decision":"deny", "source":"policy_unavailable", "id":null}),
            );
        receipt.result["authority"] = json!({"source":"policy", "policy":policy});
        receipts::persist(self.runtime(), receipt).await?;
        if policy["decision"] != "allow" {
            return Err(Failure::refused("not_opener"));
        }
        Ok(json!({"source":"policy", "policy":policy}))
    }

    pub(super) async fn change_pr(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        verb: &str,
        params: &Value,
        receipt: &mut Receipt,
        opened: Opened<'_>,
    ) -> Result<Value, Failure> {
        let n = number(params)?;
        let expected = required(params, "expected_head")?.to_ascii_lowercase();
        receipt.result = json!({"number":n, "head_sha":expected});
        let authority = self
            .pr_change_authority(token, registry, verb, receipt, &opened)
            .await?;
        let fields = if verb == "git.pr_edit" {
            edit_fields(params)?
        } else {
            Map::from_iter([("state".to_string(), json!("closed"))])
        };
        receipt.result = json!({"number":n, "head_sha":expected, "authority":authority});
        receipts::persist(self.runtime(), receipt).await?;
        let reply = self
            .api(
                opened.secret,
                "PATCH",
                endpoint(&opened.target.slug, &format!("pulls/{n}")),
                Some(Value::Object(fields.clone())),
            )
            .await?;
        // The write was sent; from here a reply that does not show it is an unknown outcome.
        let unknown = |_| Failure::unknown("remote_response");
        if reply.get("number").and_then(Value::as_u64) != Some(n) {
            return Err(Failure::unknown("remote_response"));
        }
        let head = observed_sha(&reply, "/head/sha").map_err(unknown)?;
        let state = observed_string(&reply, "/state").map_err(unknown)?;
        let merged = reply.get("merged").and_then(Value::as_bool);
        let mut result = json!({"number":n, "head_sha":head, "state":state, "authority":authority});
        if verb == "git.pr_edit" {
            for (field, value) in &fields {
                // The platform answers an empty body as null.
                let now = match reply.get(field) {
                    Some(Value::Null) if field == "body" => "",
                    Some(Value::String(now)) => now.as_str(),
                    _ => return Err(Failure::unknown("remote_response")),
                };
                if Some(now) != value.as_str() {
                    return Err(Failure::unknown("remote_response"));
                }
            }
            result["edited"] = json!(fields.keys().collect::<Vec<_>>());
        } else if state != "closed" || merged != Some(false) {
            return Err(Failure::unknown("remote_response"));
        }
        receipt.result = result.clone();
        if head != expected {
            return Err(Failure::unknown("changed_head_mismatch"));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::validate;
    use serde_json::json;

    fn reason(title: &str) -> Option<&'static str> {
        let params = json!({"number": 1, "title": title, "expected_head": "a".repeat(40)});
        validate("git.pr_edit", &params)
            .err()
            .map(|failure| failure.reason)
    }

    #[test]
    fn a_title_the_platform_could_store_altered_is_refused() {
        for title in [
            " leading",
            "trailing ",
            "   ",
            "\ttab",
            "two\nlines",
            "carriage\rreturn",
        ] {
            assert_eq!(reason(title), Some("invalid_params"), "{title:?}");
        }
        for title in ["Plain title", "Inner  spaces stay", "fix(git): one word"] {
            assert_eq!(reason(title), None, "{title:?}");
        }
    }
}
