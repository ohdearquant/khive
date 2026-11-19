use super::*;

// A child module lives in the type namespace, so it does not shadow a constant
// of the same name that a glob import brings into the value namespace.
#[test]
fn a_child_module_does_not_shadow_a_glob_imported_constant_of_the_same_name() {
    let sources = |writer_children: &str| {
        let mut sources: Vec<(String, String)> = vec![
            ("sample/src/lib.rs".into(), "mod other; mod writer;".into()),
            (
                "sample/src/other.rs".into(),
                "pub use khive_db::stores::note::NOTE_UPSERT_SQL as merge;".into(),
            ),
            (
                "sample/src/writer.rs".into(),
                format!(
                    "use crate::other::*; {writer_children}
                     fn write(conn: &Connection) {{ conn.prepare_cached(merge); }}"
                ),
            ),
        ];
        if !writer_children.is_empty() {
            sources.push((
                "sample/src/writer/merge.rs".into(),
                "pub fn unrelated() {}".into(),
            ));
        }
        sources
    };

    for children in ["mod merge;", ""] {
        let routes = scan_sources(&sources(children)).unwrap();
        assert_eq!(routes.len(), 1, "children {children:?}: {routes:?}");
        assert_eq!(routes[0].key, "sample/src/writer.rs::write");
        assert!(routes[0].evidence.contains("SQL constant NOTE_UPSERT_SQL"));
    }
}
