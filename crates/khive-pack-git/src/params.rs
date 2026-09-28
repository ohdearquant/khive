//! Closed argument objects at each handler's established refusal boundary.
//!
//! Values deliberately retain their JSON representation: the established
//! handlers own their type, null, range and cross-field rules. In particular,
//! omission is not interchangeable with null (`tree` routing and
//! `expected_remote`), and digest's signed budget must keep its clamp policy.

use khive_runtime::RuntimeError;
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Default)]
enum Supplied {
    #[default]
    Absent,
    Value(Value),
}

impl Supplied {
    fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

impl<'de> Deserialize<'de> for Supplied {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::Value)
    }
}

impl Serialize for Supplied {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Value(value) => value.serialize(serializer),
            Self::Absent => serializer.serialize_none(),
        }
    }
}

macro_rules! arguments {
    ($name:ident { $($field:ident),+ $(,)? }) => {
        #[derive(Deserialize, Serialize)]
        #[serde(deny_unknown_fields)]
        struct $name {
            $(
                #[serde(default, skip_serializing_if = "Supplied::is_absent")]
                $field: Supplied,
            )+
        }
    };
}

arguments!(Digest {
    source,
    project,
    max_items,
    include
});
arguments!(IngestCursor {
    project,
    source_kind
});
arguments!(Commit {
    repo,
    message,
    paths,
    author,
    tree,
    branch,
    expected_head,
    session_id
});
arguments!(Branch {
    repo,
    name,
    from,
    expected,
    session_id
});
arguments!(UpdateRef {
    repo,
    branch,
    to,
    expected,
    require_fast_forward,
    reason,
    session_id
});
arguments!(Push {
    repo,
    branch,
    expected_local,
    expected_remote,
    session_id
});
arguments!(PrOpen {
    repo,
    head,
    base,
    title,
    body,
    expected_head,
    session_id
});
arguments!(PrReview {
    repo,
    number,
    verdict,
    body,
    expected_head,
    session_id
});
arguments!(PrMerge {
    repo,
    number,
    method,
    subject,
    body,
    expected_head,
    session_id
});
arguments!(Init {
    repo,
    branch,
    session_id
});
arguments!(Checkout {
    repo,
    r#ref,
    session_id
});
arguments!(Diff {
    repo,
    input_kind,
    base,
    head,
    session_id
});
arguments!(Reconcile { receipt });
arguments!(Receipts {
    repo,
    actor,
    session_id,
    limit,
    offset
});
arguments!(Gates { repo });
arguments!(Status {
    repo,
    untracked,
    limit
});
arguments!(Log {
    repo,
    r#ref,
    limit,
    path
});

fn decode<T: DeserializeOwned + Serialize>(
    verb: &str,
    params: Value,
) -> Result<Value, RuntimeError> {
    let args: T = serde_json::from_value(params)
        .map_err(|error| RuntimeError::InvalidInput(format!("{verb}: {error}")))?;
    serde_json::to_value(args)
        .map_err(|error| RuntimeError::Internal(format!("{verb} arguments: {error}")))
}

pub(crate) fn parse(verb: &str, params: Value) -> Result<Value, RuntimeError> {
    if !params.is_object() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb} arguments must be an object with named fields"
        )));
    }
    match verb {
        "git.digest" => decode::<Digest>(verb, params),
        "git.ingest_cursor" => decode::<IngestCursor>(verb, params),
        "git.commit" => decode::<Commit>(verb, params),
        "git.branch" => decode::<Branch>(verb, params),
        "git.update_ref" => decode::<UpdateRef>(verb, params),
        "git.push" => decode::<Push>(verb, params),
        "git.pr_open" => decode::<PrOpen>(verb, params),
        "git.pr_review" => decode::<PrReview>(verb, params),
        "git.pr_merge" => decode::<PrMerge>(verb, params),
        "git.init" => decode::<Init>(verb, params),
        "git.checkout" => decode::<Checkout>(verb, params),
        "git.diff" => decode::<Diff>(verb, params),
        "git.reconcile" => decode::<Reconcile>(verb, params),
        "git.receipts" => decode::<Receipts>(verb, params),
        "git.gates" => decode::<Gates>(verb, params),
        "git.status" => decode::<Status>(verb, params),
        "git.log" => decode::<Log>(verb, params),
        _ => Err(RuntimeError::InvalidInput(format!(
            "git pack does not handle verb {verb:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use serde_json::json;

    #[test]
    fn known_values_keep_null_presence_and_existing_semantic_validation() {
        for (verb, params) in [
            (
                "git.digest",
                json!({"source":"repo","project":false,"max_items":null,"include":null}),
            ),
            ("git.digest", json!({"source":"repo","max_items":-1})),
            ("git.digest", json!({"source":"repo","max_items":9000})),
            (
                "git.commit",
                json!({"repo":"repo","message":"message","paths":null,"author":null}),
            ),
            (
                "git.commit",
                json!({"repo":"repo","message":"message","tree":null}),
            ),
            ("git.push", json!({"repo":"repo","expected_remote":null})),
            ("git.push", json!({"repo":"repo"})),
            ("git.log", json!({"repo":"repo","ref":"HEAD","limit":null})),
        ] {
            assert_eq!(
                parse(verb, params.clone()).unwrap(),
                params,
                "SUPPLIED_VALUES_PRESERVED: {verb}"
            );
        }
    }

    #[test]
    fn positional_arguments_are_not_named_objects() {
        for verb in ["git.digest", "git.commit", "git.init", "git.push"] {
            let error = parse(verb, json!([]))
                .expect_err("NAMED_ARGUMENTS_ONLY: positional array accepted");
            assert!(
                error.to_string().contains("object with named fields"),
                "NAMED_ARGUMENTS_ONLY: {error}"
            );
        }
    }
}
