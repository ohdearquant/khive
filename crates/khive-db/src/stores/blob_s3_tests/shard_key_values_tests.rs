#[test]
fn shard_key_matches_fs_blob_store_shape() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
    let key = store.shard_key(&content_ref);
    let key_str: &str = key.as_ref();
    assert_eq!(key_str, format!("blobs/aa/aa/{}", "a".repeat(64)));
}

#[test]
fn parse_shard_key_roundtrips_a_valid_key() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let content_ref = ContentRef::from_hex("b".repeat(64)).unwrap();
    let key = store.shard_key(&content_ref);
    assert_eq!(store.parse_shard_key(&key), Some(content_ref));
}

#[test]
fn parse_shard_key_rejects_a_foreign_key_under_the_prefix() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let foreign = ObjectPath::from("blobs/README.txt");
    assert_eq!(store.parse_shard_key(&foreign), None);
}

#[test]
fn parse_shard_key_rejects_mismatched_shard_segments() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let hex = "c".repeat(64);
    // Shard segments deliberately don't match the hex's own prefix.
    let bad = ObjectPath::from(format!("blobs/00/00/{hex}"));
    assert_eq!(store.parse_shard_key(&bad), None);
}

#[test]
fn parse_shard_key_rejects_extra_path_segments() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let hex = "d".repeat(64);
    let bad = ObjectPath::from(format!("blobs/dd/dd/{hex}/extra"));
    assert_eq!(store.parse_shard_key(&bad), None);
}

#[test]
fn parse_shard_key_rejects_key_outside_the_configured_prefix() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let hex = "e".repeat(64);
    let bad = ObjectPath::from(format!("other-prefix/ee/ee/{hex}"));
    assert_eq!(store.parse_shard_key(&bad), None);
}
