use std::sync::{Arc, Mutex};

use crate::channel_section::{ChannelConfigValidators, ChannelSectionConfig};
use crate::{runtime_config_from_khive_config, KhiveConfig, RuntimeConfig, RuntimeError};

#[derive(serde::Deserialize, serde::Serialize)]
struct Doc {
    #[serde(default)]
    channel: ChannelSectionConfig,
}

fn parse(text: &str) -> ChannelSectionConfig {
    toml::from_str::<Doc>(text)
        .expect("channel configuration")
        .channel
}

#[test]
fn absent_channel_section_is_empty_and_valid() {
    let section = parse("");
    assert!(section.is_empty());
    assert!(section.get("stand_in").is_none());
    section.validate().expect("absent section is valid");
    ChannelConfigValidators::default()
        .validate(&section)
        .expect("no registered component");
}

#[test]
fn channel_values_remain_lossless_and_unregistered_values_are_inert() {
    let section = parse(
        r#"
[channel]
legacy = 17
[channel.stand_in]
enabled = true
label = "e\u0301\nverbatim"
weights = [1, 2.5]
[[channel.stand_in.pins]]
agent = "not interpreted"
[channel.other]
unsupported_by_component = false
"#,
    );
    let expected = toml::Value::Table(toml::Table::from_iter([
        ("enabled".into(), toml::Value::Boolean(true)),
        (
            "label".into(),
            toml::Value::String("e\u{301}\nverbatim".into()),
        ),
        (
            "weights".into(),
            toml::Value::Array(vec![toml::Value::Integer(1), toml::Value::Float(2.5)]),
        ),
        (
            "pins".into(),
            toml::Value::Array(vec![toml::Value::Table(toml::Table::from_iter([(
                "agent".into(),
                toml::Value::String("not interpreted".into()),
            )]))]),
        ),
    ]));
    assert_eq!(
        section.get("stand_in"),
        Some(&expected),
        "opaque channel values must retain every literal nested field"
    );
    assert_eq!(section.get("legacy"), Some(&toml::Value::Integer(17)));
    assert_eq!(
        section.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        ["legacy", "other", "stand_in",]
    );
    section.validate().expect("unregistered values are opaque");
    ChannelConfigValidators::default()
        .validate(&section)
        .expect("unregistered component values have no effect");
    let encoded = toml::to_string(&Doc {
        channel: section.clone(),
    })
    .expect("serialize section");
    assert_eq!(parse(&encoded), section, "opaque values must round-trip");
}

#[test]
fn registered_validators_receive_only_their_exact_optional_value() {
    let section = parse("[channel]\npresent = 23\nsibling = 'untouched'\n");
    let observations = Arc::new(Mutex::new(Vec::new()));
    let mut validators = ChannelConfigValidators::default();
    for name in ["present", "absent"] {
        let observations = observations.clone();
        validators
            .register(
                name,
                Arc::new(move |value| {
                    observations.lock().unwrap().push((name, value.cloned()));
                    Ok(())
                }),
            )
            .unwrap();
    }
    validators.validate(&section).unwrap();
    assert_eq!(
        *observations.lock().unwrap(),
        vec![
            ("absent", None),
            ("present", Some(toml::Value::Integer(23)))
        ],
        "registered validators must receive exact values and absent names as None"
    );
    assert_eq!(
        section.get("sibling").and_then(toml::Value::as_str),
        Some("untouched")
    );
}

#[test]
fn duplicate_registration_preserves_the_first_callback_and_clones_are_independent() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut validators = ChannelConfigValidators::default();
    let first_calls = calls.clone();
    validators
        .register(
            "stand_in",
            Arc::new(move |_| {
                first_calls.lock().unwrap().push("first");
                Ok(())
            }),
        )
        .unwrap();
    let mut cloned = validators.clone();
    let replacement_calls = calls.clone();
    let duplicate = validators.register(
        "stand_in",
        Arc::new(move |_| {
            replacement_calls.lock().unwrap().push("replacement");
            Ok(())
        }),
    );
    assert!(
        matches!(duplicate, Err(RuntimeError::InvalidInput(ref message)) if message.contains("already registered")),
        "duplicate component registration must refuse without replacing its callback"
    );
    let extra_calls = calls.clone();
    cloned
        .register(
            "extra",
            Arc::new(move |_| {
                extra_calls.lock().unwrap().push("extra");
                Ok(())
            }),
        )
        .unwrap();
    validators
        .validate(&ChannelSectionConfig::default())
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), ["first"]);
    cloned.validate(&ChannelSectionConfig::default()).unwrap();
    assert_eq!(*calls.lock().unwrap(), ["first", "extra", "first"]);
    assert!(ChannelConfigValidators::default()
        .register("", Arc::new(|_| Ok(())))
        .is_err());
}

#[test]
fn component_shape_refusal_remains_typed_and_debug_omits_callback_state() {
    let section = parse("[channel]\nstand_in = 7\n");
    let mut validators = ChannelConfigValidators::default();
    let captured_secret = "private-callback-state".to_owned();
    validators
        .register(
            "stand_in",
            Arc::new(move |value| {
                let _ = &captured_secret;
                if value.and_then(toml::Value::as_table).is_none() {
                    return Err(RuntimeError::InvalidInput(
                        "stand_in requires a table".into(),
                    ));
                }
                Ok(())
            }),
        )
        .unwrap();
    let result = validators.validate(&section);
    assert!(
        matches!(result, Err(RuntimeError::InvalidInput(ref message)) if message == "stand_in requires a table"),
        "a registered component's typed validation refusal must propagate"
    );
    assert_eq!(
        format!("{validators:?}"),
        "ChannelConfigValidators { names: [\"stand_in\"] }"
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn channel_values_and_registered_callbacks_survive_both_runtime_conversion_paths() {
    for engines in [
        "",
        "\n[[engines]]\nname = 'main'\nmodel = 'all-minilm-l6-v2'\ndefault = true\n",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            format!("[channel.stand_in]\nlabel = 'literal input'\n{engines}"),
        )
        .unwrap();
        let loaded = KhiveConfig::load(Some(&path)).unwrap().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let mut base = RuntimeConfig {
            db_path: None,
            ..RuntimeConfig::no_embeddings()
        };
        assert!(
            base.channel.is_empty(),
            "current runtime default has no configured channel"
        );
        base.channel_config_validators
            .register(
                "stand_in",
                Arc::new(move |value| {
                    observed.lock().unwrap().push(value.cloned());
                    Ok(())
                }),
            )
            .unwrap();
        let resolved = runtime_config_from_khive_config(&loaded, base);
        resolved
            .channel_config_validators
            .validate(&resolved.channel)
            .unwrap();
        let expected = toml::Value::Table(toml::Table::from_iter([(
            "label".into(),
            toml::Value::String("literal input".into()),
        )]));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![Some(expected)],
            "both runtime conversion branches must preserve the opaque value and registered callback"
        );
        assert_eq!(resolved.channel, loaded.channel);
    }
}
