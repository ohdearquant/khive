//! Per-event missed-schedule policy shared by creation and the executor.

use chrono::Duration;
use serde_json::Value;

/// The validated policy in a scheduled event's managed properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MisfirePolicy {
    /// Preserve the executor's existing global grace and skip behavior.
    Skip,
    /// Consider only the latest due occurrence within this explicit bound.
    CatchUpOnce {
        /// Required positive, representable integer seconds.
        max_lateness: i64,
    },
}

/// Validate creation arguments or stored properties using the same contract.
/// Absent policy means skip; explicit nulls and wrong types are invalid.
pub fn parse_misfire_policy(properties: &Value) -> Result<MisfirePolicy, String> {
    let policy = match properties.get("misfire_policy") {
        None => "skip",
        Some(value) => value
            .as_str()
            .ok_or_else(|| "misfire_policy must be \"skip\" or \"catch_up_once\"".to_string())?,
    };
    let max_lateness = properties
        .get("max_lateness")
        .map(|value| {
            value
                .as_i64()
                .filter(|seconds| *seconds > 0)
                .filter(|seconds| Duration::try_seconds(*seconds).is_some())
                .ok_or_else(|| {
                    "max_lateness must be a representable positive integer number of seconds"
                        .to_string()
                })
        })
        .transpose()?;
    match (policy, max_lateness) {
        ("skip", None) => Ok(MisfirePolicy::Skip),
        ("skip", Some(_)) => Err("max_lateness is only allowed with catch_up_once".into()),
        ("catch_up_once", Some(max_lateness)) => Ok(MisfirePolicy::CatchUpOnce { max_lateness }),
        ("catch_up_once", None) => {
            Err("catch_up_once requires an explicit positive max_lateness".into())
        }
        _ => Err("misfire_policy must be \"skip\" or \"catch_up_once\"".into()),
    }
}

/// Validated declaration-only policy for version-three occurrence accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct V3MisfirePolicy {
    kind: MisfirePolicy,
    requested_max_lateness_s: Option<i64>,
}

impl V3MisfirePolicy {
    pub(crate) fn kind(self) -> MisfirePolicy {
        self.kind
    }

    pub(crate) fn requested_max_lateness_s(self) -> Option<i64> {
        self.requested_max_lateness_s
    }
}

/// Validate the raw version-three fields before optional serde fields erase nulls.
pub(crate) fn parse_v3_misfire_policy(properties: &Value) -> Result<V3MisfirePolicy, String> {
    let policy = match properties.get("misfire_policy") {
        None => "skip",
        Some(value) => value
            .as_str()
            .ok_or_else(|| "misfire_policy must be \"skip\" or \"catch_up_once\"".to_string())?,
    };
    if policy != "skip" {
        let kind = parse_misfire_policy(properties)?;
        let requested_max_lateness_s = match kind {
            MisfirePolicy::CatchUpOnce { max_lateness } => Some(max_lateness),
            MisfirePolicy::Skip => None,
        };
        return Ok(V3MisfirePolicy {
            kind,
            requested_max_lateness_s,
        });
    }
    let requested_max_lateness_s = properties
        .get("max_lateness")
        .map(|value| {
            value
                .as_i64()
                .filter(|seconds| *seconds >= 0)
                .filter(|seconds| Duration::try_seconds(*seconds).is_some())
                .ok_or_else(|| {
                    "max_lateness must be a representable nonnegative integer number of seconds"
                        .to_string()
                })
        })
        .transpose()?;
    Ok(V3MisfirePolicy {
        kind: MisfirePolicy::Skip,
        requested_max_lateness_s,
    })
}
