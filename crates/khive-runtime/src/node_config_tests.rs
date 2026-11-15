use crate::node_config::ChannelSectionConfig;

#[derive(serde::Deserialize)]
struct Doc {
    #[serde(default)]
    channel: ChannelSectionConfig,
}

fn parse(text: &str) -> Result<ChannelSectionConfig, toml::de::Error> {
    toml::from_str::<Doc>(text).map(|doc| doc.channel)
}

#[test]
fn an_absent_channel_section_has_no_node_table_and_validates() {
    let section = parse("").expect("empty document");
    assert!(section.node.is_none());
    section.validate().expect("absent table is valid");
}

#[test]
fn a_node_table_still_loads_and_is_not_interpreted() {
    let section = parse(
        "[channel.node]\nenabled = true\nslug = \"not checked\"\nbatch_size = 9999\n\n[[channel.node.pins]]\nagent_id = \"anything\"\n",
    )
    .expect("a node table still parses");
    assert!(section.node.is_some());
    section
        .validate()
        .expect("a node table is accepted as it stands");
}

#[test]
fn the_channel_section_still_rejects_unknown_keys() {
    assert!(parse("[channel]\nother = 1\n").is_err());
}
