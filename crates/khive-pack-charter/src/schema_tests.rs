use super::CHARTER_SCHEMA_STATEMENTS;

#[test]
fn every_schema_entry_opens_with_its_one_table() {
    let mut tables = Vec::new();
    for entry in CHARTER_SCHEMA_STATEMENTS {
        let first = entry.lines().next().unwrap_or_default();
        assert!(
            first.starts_with("CREATE TABLE IF NOT EXISTS charter_"),
            "schema entry does not open with its table: {first}"
        );
        assert_eq!(
            entry.matches("CREATE TABLE").count(),
            1,
            "schema entry declares more than one table: {first}"
        );
        tables.push(first.split_whitespace().nth(5).unwrap_or_default());
    }
    tables.sort_unstable();
    tables.dedup();
    assert_eq!(tables.len(), 8, "{tables:?}");
}
