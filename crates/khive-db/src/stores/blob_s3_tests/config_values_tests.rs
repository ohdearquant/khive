#[test]
fn config_rejects_empty_bucket() {
    let mut cfg = valid_config();
    cfg.bucket = String::new();
    assert!(cfg.validate().is_err());
}

#[test]
fn config_rejects_empty_region() {
    let mut cfg = valid_config();
    cfg.region = String::new();
    assert!(cfg.validate().is_err());
}

#[test]
fn config_default_prefix_is_blobs() {
    assert_eq!(valid_config().prefix, "blobs");
}

#[test]
fn config_rejects_leading_slash_prefix() {
    let cfg = valid_config().with_prefix("/blobs");
    assert!(cfg.validate().is_err());
}

#[test]
fn config_rejects_trailing_slash_prefix() {
    let cfg = valid_config().with_prefix("blobs/");
    assert!(cfg.validate().is_err());
}

#[test]
fn config_rejects_empty_prefix() {
    let cfg = valid_config().with_prefix("");
    assert!(cfg.validate().is_err());
}

#[test]
fn config_rejects_dot_segment_in_prefix() {
    let cfg = valid_config().with_prefix("a/./b");
    assert!(cfg.validate().is_err());
}

#[test]
fn config_rejects_dotdot_segment_in_prefix() {
    let cfg = valid_config().with_prefix("a/../b");
    assert!(cfg.validate().is_err());
}

#[test]
fn config_rejects_double_slash_prefix() {
    let cfg = valid_config().with_prefix("a//b");
    assert!(cfg.validate().is_err());
}

#[test]
fn config_accepts_nested_prefix() {
    let cfg = valid_config().with_prefix("a/b/c");
    assert!(cfg.validate().is_ok());
}
