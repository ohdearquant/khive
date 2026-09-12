use serde::{Deserialize, Serialize};

use crate::engine_config::ConfigError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryCarrier {
    Durable,
    Ephemeral,
}

impl TelemetryCarrier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Durable => "durable",
            Self::Ephemeral => "ephemeral",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryFailurePosture {
    Stop,
    Gap,
}

impl TelemetryFailurePosture {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Gap => "gap",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TelemetryPolicy {
    pub classified: bool,
    pub carrier: TelemetryCarrier,
    pub failure_posture: TelemetryFailurePosture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TelemetryChannelConfig {
    pub kinds: Vec<String>,
    pub carrier: TelemetryCarrier,
    pub failure_posture: TelemetryFailurePosture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawTelemetryConfig")]
pub struct TelemetryConfig {
    pub stream: String,
    pub default_carrier: Option<TelemetryCarrier>,
    pub channels: Vec<TelemetryChannelConfig>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            stream: default_stream(),
            default_carrier: None,
            channels: Vec::new(),
        }
    }
}

impl TelemetryConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.stream.len() > 512 || self.stream.contains('\0') {
            return Err(invalid(
                "telemetry.stream",
                "must be at most 512 UTF-8 bytes and contain no U+0000",
            ));
        }
        for (index, channel) in self.channels.iter().enumerate() {
            let entry = format!("telemetry.channels[{index}]");
            if channel.kinds.is_empty() {
                return Err(invalid(&entry, "kinds must not be empty"));
            }
            for kind in &channel.kinds {
                let suffix = kind.strip_prefix("*.");
                if kind.is_empty()
                    || kind.chars().any(|c| c.is_whitespace() || c.is_control())
                    || (kind.contains('*')
                        && !suffix.is_some_and(|s| !s.is_empty() && !s.contains('*')))
                {
                    return Err(invalid(
                        &entry,
                        format!(
                            "invalid kind pattern {kind:?}; expected a nonempty exact name or *.suffix without whitespace or control characters"
                        ),
                    ));
                }
                for (previous_index, previous) in self.channels[..index].iter().enumerate() {
                    for previous_kind in &previous.kinds {
                        if patterns_overlap(kind, previous_kind) {
                            return Err(invalid(
                                &entry,
                                format!(
                                    "kind pattern {kind:?} overlaps {previous_kind:?} in telemetry.channels[{previous_index}]"
                                ),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn validate_activation(&self) -> Result<(), ConfigError> {
        self.declared_default()?;
        self.validate()
    }

    fn declared_default(&self) -> Result<TelemetryCarrier, ConfigError> {
        self.default_carrier.ok_or_else(|| {
            invalid(
                "telemetry.default_carrier",
                "must be explicitly declared when the telemetry pack is loaded",
            )
        })
    }

    pub fn policy_for_kind(&self, kind: &str) -> Result<TelemetryPolicy, ConfigError> {
        let default_carrier = self.declared_default()?;
        for channel in &self.channels {
            if channel
                .kinds
                .iter()
                .any(|pattern| pattern_matches(pattern, kind))
            {
                return Ok(TelemetryPolicy {
                    classified: true,
                    carrier: channel.carrier,
                    failure_posture: channel.failure_posture,
                });
            }
        }
        Ok(TelemetryPolicy {
            classified: false,
            carrier: default_carrier,
            failure_posture: match default_carrier {
                TelemetryCarrier::Durable => TelemetryFailurePosture::Stop,
                TelemetryCarrier::Ephemeral => TelemetryFailurePosture::Gap,
            },
        })
    }
}

fn pattern_matches(pattern: &str, kind: &str) -> bool {
    match pattern.strip_prefix('*') {
        Some(suffix) => kind.ends_with(suffix),
        None => pattern == kind,
    }
}

fn patterns_overlap(first: &str, second: &str) -> bool {
    match (first.strip_prefix('*'), second.strip_prefix('*')) {
        (Some(first), Some(second)) => first.ends_with(second) || second.ends_with(first),
        (Some(_), None) => pattern_matches(first, second),
        (None, Some(_)) => pattern_matches(second, first),
        (None, None) => first == second,
    }
}

fn invalid(entry: impl Into<String>, reason: impl Into<String>) -> ConfigError {
    ConfigError::InvalidTelemetryConfig {
        entry: entry.into(),
        reason: reason.into(),
    }
}

fn default_stream() -> String {
    "telemetry".to_string()
}

// Decode policy values with the enclosing channel index available for diagnostics.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTelemetryConfig {
    #[serde(default = "default_stream")]
    stream: String,
    #[serde(default)]
    default_carrier: Option<String>,
    #[serde(default)]
    channels: Vec<RawTelemetryChannelConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTelemetryChannelConfig {
    kinds: Vec<String>,
    carrier: String,
    failure_posture: String,
}

fn parse_carrier(value: &str, entry: &str) -> Result<TelemetryCarrier, ConfigError> {
    match value {
        "durable" => Ok(TelemetryCarrier::Durable),
        "ephemeral" => Ok(TelemetryCarrier::Ephemeral),
        _ => Err(invalid(
            entry,
            format!("unknown carrier {value:?}; expected durable or ephemeral"),
        )),
    }
}

impl TryFrom<RawTelemetryConfig> for TelemetryConfig {
    type Error = ConfigError;

    fn try_from(raw: RawTelemetryConfig) -> Result<Self, Self::Error> {
        let default_carrier = raw
            .default_carrier
            .as_deref()
            .map(|value| parse_carrier(value, "telemetry.default_carrier"))
            .transpose()?;
        let mut channels = Vec::with_capacity(raw.channels.len());
        for (index, channel) in raw.channels.into_iter().enumerate() {
            let entry = format!("telemetry.channels[{index}]");
            let carrier = parse_carrier(&channel.carrier, &entry)?;
            let failure_posture = match channel.failure_posture.as_str() {
                "stop" => TelemetryFailurePosture::Stop,
                "gap" => TelemetryFailurePosture::Gap,
                value => {
                    return Err(invalid(
                        &entry,
                        format!("unknown failure_posture {value:?}; expected stop or gap"),
                    ));
                }
            };
            channels.push(TelemetryChannelConfig {
                kinds: channel.kinds,
                carrier,
                failure_posture,
            });
        }
        let config = Self {
            stream: raw.stream,
            default_carrier,
            channels,
        };
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_default_is_structurally_valid_but_cannot_activate() {
        for text in ["", "stream = \"events\"\n"] {
            let missing: TelemetryConfig = toml::from_str(text).unwrap();
            assert_eq!(missing.default_carrier, None);
            missing.validate().unwrap();
            assert!(missing
                .validate_activation()
                .unwrap_err()
                .to_string()
                .contains("telemetry.default_carrier"));
            assert!(missing.policy_for_kind("unclassified").is_err());
            assert_eq!(
                serde_json::from_value::<TelemetryConfig>(serde_json::to_value(&missing).unwrap())
                    .unwrap(),
                missing
            );
        }
        for carrier in ["durable", "ephemeral"] {
            let declared: TelemetryConfig =
                toml::from_str(&format!("default_carrier = {carrier:?}")).unwrap();
            declared.validate_activation().unwrap();
            assert!(!declared.policy_for_kind("unclassified").unwrap().classified);
        }
    }

    #[test]
    fn classification_tracks_matching_even_when_the_carrier_equals_the_default() {
        for carrier in [TelemetryCarrier::Durable, TelemetryCarrier::Ephemeral] {
            let config = TelemetryConfig {
                default_carrier: Some(carrier),
                channels: vec![TelemetryChannelConfig {
                    kinds: vec!["*.heartbeat".into(), "run.started".into()],
                    carrier,
                    failure_posture: TelemetryFailurePosture::Stop,
                }],
                ..TelemetryConfig::default()
            };
            let round_trip: TelemetryConfig =
                serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
            for kind in ["run.heartbeat", "run.started", "unclassified"] {
                let policy = config.policy_for_kind(kind).unwrap();
                assert_eq!(policy.carrier, carrier);
                assert_eq!(policy.classified, kind != "unclassified");
                assert_eq!(round_trip.policy_for_kind(kind).unwrap(), policy);
            }
        }
    }

    #[test]
    fn directly_constructed_config_rejects_ambiguous_channels() {
        let mut config = TelemetryConfig {
            channels: vec![
                TelemetryChannelConfig {
                    kinds: vec!["*.heartbeat".to_string()],
                    carrier: TelemetryCarrier::Ephemeral,
                    failure_posture: TelemetryFailurePosture::Gap,
                },
                TelemetryChannelConfig {
                    kinds: vec!["run.heartbeat".to_string()],
                    carrier: TelemetryCarrier::Durable,
                    failure_posture: TelemetryFailurePosture::Stop,
                },
            ],
            ..TelemetryConfig::default()
        };
        let error = config.validate().expect_err("overlap must refuse");
        assert!(matches!(
            error,
            ConfigError::InvalidTelemetryConfig { entry, .. }
                if entry == "telemetry.channels[1]"
        ));

        config.channels[1].kinds.clear();
        let error = config.validate().expect_err("empty channel must refuse");
        assert!(
            error.to_string().contains("telemetry.channels[1]"),
            "{error}"
        );
    }

    #[test]
    fn disjoint_suffixes_and_same_channel_patterns_remain_valid() {
        let config: TelemetryConfig = toml::from_str(
            r#"
default_carrier = "durable"
[[channels]]
kinds = ["*.heartbeat", "run.heartbeat"]
carrier = "ephemeral"
failure_posture = "gap"
[[channels]]
kinds = ["*.notheartbeat", "heartbeat"]
carrier = "durable"
failure_posture = "stop"
"#,
        )
        .expect("distinct suffixes have no common kind");
        assert_eq!(
            config.policy_for_kind("run.heartbeat").unwrap().carrier,
            TelemetryCarrier::Ephemeral
        );
        assert_eq!(
            config.policy_for_kind("run.notheartbeat").unwrap().carrier,
            TelemetryCarrier::Durable
        );
        assert_eq!(
            config.policy_for_kind("heartbeat").unwrap().carrier,
            TelemetryCarrier::Durable
        );
    }

    #[test]
    fn config_serializes_effective_policy_and_round_trips() {
        let config = TelemetryConfig {
            default_carrier: Some(TelemetryCarrier::Ephemeral),
            channels: vec![TelemetryChannelConfig {
                kinds: vec!["run.started".to_string()],
                carrier: TelemetryCarrier::Durable,
                failure_posture: TelemetryFailurePosture::Stop,
            }],
            ..TelemetryConfig::default()
        };
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(value["stream"], "telemetry");
        assert_eq!(value["default_carrier"], "ephemeral");
        assert_eq!(value["channels"][0]["carrier"], "durable");
        assert_eq!(value["channels"][0]["failure_posture"], "stop");
        assert_eq!(
            serde_json::from_value::<TelemetryConfig>(value).unwrap(),
            config
        );
    }
}
