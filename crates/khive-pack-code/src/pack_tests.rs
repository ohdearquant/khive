use khive_types::Pack;

use super::CodePack;

#[test]
fn code_pack_declares_adr085_metadata() {
    assert_eq!(<CodePack as Pack>::NAME, "code");
    assert_eq!(<CodePack as Pack>::NOTE_KINDS, &["finding"]);
    assert!(<CodePack as Pack>::ENTITY_KINDS.is_empty());
    assert_eq!(<CodePack as Pack>::HANDLERS.len(), 1);
    assert_eq!(<CodePack as Pack>::HANDLERS[0].name, "code.ingest");
    assert_eq!(<CodePack as Pack>::REQUIRES, &["kg"]);
    let plan = <CodePack as Pack>::SCHEMA_PLAN.expect("named empty schema plan");
    assert_eq!(plan.pack, "code");
    assert!(plan.statements.is_empty());
}
