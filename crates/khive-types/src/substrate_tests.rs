use super::*;

#[test]
fn all_variants() {
    assert_eq!(SubstrateKind::ALL.len(), SUBSTRATE_COUNT);
    for (i, &kind) in SubstrateKind::ALL.iter().enumerate() {
        assert_eq!(kind as u8, i as u8);
        assert_eq!(SubstrateKind::from_u8(i as u8), Some(kind));
    }
}

#[test]
fn parse_roundtrip() {
    for kind in SubstrateKind::ALL {
        let parsed: SubstrateKind = kind.name().parse().unwrap();
        assert_eq!(parsed, kind);
    }
}

#[test]
fn out_of_range() {
    assert_eq!(SubstrateKind::from_u8(3), None);
    assert_eq!(SubstrateKind::from_u8(255), None);
}
