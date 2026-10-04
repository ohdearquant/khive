use super::pending_removed_tests::{fixture, pending_file};
use super::*;

const CLEAN_DECLARATION: &str = "fn clean(){crate::b::ghost();}\n";

#[tokio::test]
async fn l2_refused_reference_does_not_refuse_the_files_coverage() {
    for wal in [true, false] {
        let (_dir, root, rt, token) = fixture(wal);
        let refused = ["AKIA", "1234567890ABCDEF"].concat();
        let text = format!("fn helper(){{{refused}();}}\n{CLEAN_DECLARATION}");
        source(&root, "a.rs", &text);
        let (report, _) = l2(&rt, &token, &root, 10).await;
        // The per-item screen reports the refused reference once; the module
        // stamp leaves it out instead of refusing (and counting) the whole row.
        assert_eq!(report.blocked_count, 1);
        assert_eq!(report.blocked.len(), 1);
        let module = stored(&rt, &token, module_uuid("fixture", "rust", "a")).await;
        let props = module.properties.unwrap();
        let (clean, helper) = (symbol("a", "clean"), symbol("a", "helper"));
        let mut ids = [clean, helper];
        ids.sort();
        assert_eq!(props["declaration_ids"], json!(ids));
        assert_eq!(props["l2_content_hash"], json!(content_hash(&text)));
        let entry = pending_file(&rt, &token, &root, "a.rs", "a").await;
        let references = entry["references"].as_array().unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0]["declaration_id"], json!(clean));
        assert!(!entry.to_string().contains(&refused));
        let (_, work) = l2(&rt, &token, &root, 20).await;
        assert!(work.parsed.is_empty());
    }
}

#[tokio::test]
async fn l2_entry_written_by_another_scanner_version_is_reparsed() {
    for wal in [true, false] {
        for absent in [false, true] {
            let (_dir, root, rt, token) = fixture(wal);
            source(&root, "a.rs", "fn helper(){}\n");
            l2(&rt, &token, &root, 10).await;
            let file = root.join("a.rs").canonicalize().unwrap();
            let key = file_pending::file_key(&file.display().to_string());
            let mut module = stored(&rt, &token, module_uuid("fixture", "rust", "a")).await;
            let entry = module
                .properties
                .as_mut()
                .and_then(|props| props.get_mut("l2_file_pending"))
                .and_then(|all| all.get_mut(key.as_str()))
                .and_then(Value::as_object_mut)
                .unwrap();
            if absent {
                entry.remove("scanner_version");
            } else {
                let other = RUST_L2_SCANNER_IDENTITY_VERSION - 1;
                entry.insert("scanner_version".into(), json!(other));
            }
            rt.entities(&token)
                .unwrap()
                .upsert_entity(module)
                .await
                .unwrap();
            let (_, work) = l2(&rt, &token, &root, 20).await;
            assert!(work.parsed.contains(&file));
            let entry = pending_file(&rt, &token, &root, "a.rs", "a").await;
            assert_eq!(
                entry["scanner_version"],
                json!(RUST_L2_SCANNER_IDENTITY_VERSION)
            );
            let (_, work) = l2(&rt, &token, &root, 30).await;
            assert!(work.parsed.is_empty());
        }
    }
}
