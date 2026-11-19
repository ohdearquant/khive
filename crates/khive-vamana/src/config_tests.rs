use super::*;

#[test]
fn default_matches_adr048_values() {
    let cfg = VamanaConfig::default();
    assert_eq!(cfg.dimensions, 384);
    assert_eq!(cfg.max_degree, 64);
    assert_eq!(cfg.search_list_size, 128);
    assert_eq!(cfg.alpha, 1.2);
}

#[test]
fn validate_accepts_default() {
    assert!(VamanaConfig::default().validate().is_ok());
}

#[test]
fn validate_rejects_zero_dimensions() {
    let cfg = VamanaConfig {
        dimensions: 0,
        ..VamanaConfig::default()
    };
    assert!(matches!(
        cfg.validate(),
        Err(VamanaError::InvalidConfig { .. })
    ));
}

#[test]
fn validate_rejects_zero_max_degree() {
    let cfg = VamanaConfig {
        max_degree: 0,
        ..VamanaConfig::default()
    };
    assert!(matches!(
        cfg.validate(),
        Err(VamanaError::InvalidConfig { .. })
    ));
}

#[test]
fn validate_rejects_zero_search_list_size() {
    let cfg = VamanaConfig {
        search_list_size: 0,
        ..VamanaConfig::default()
    };
    assert!(matches!(
        cfg.validate(),
        Err(VamanaError::InvalidConfig { .. })
    ));
}

#[test]
fn validate_rejects_search_list_smaller_than_degree() {
    let cfg = VamanaConfig {
        max_degree: 64,
        search_list_size: 32,
        ..VamanaConfig::default()
    };
    assert!(matches!(
        cfg.validate(),
        Err(VamanaError::InvalidConfig { .. })
    ));
}

#[test]
fn validate_rejects_nonfinite_alpha() {
    let nan_cfg = VamanaConfig {
        alpha: f64::NAN,
        ..VamanaConfig::default()
    };
    assert!(matches!(
        nan_cfg.validate(),
        Err(VamanaError::InvalidConfig { .. })
    ));

    let inf_cfg = VamanaConfig {
        alpha: f64::INFINITY,
        ..VamanaConfig::default()
    };
    assert!(matches!(
        inf_cfg.validate(),
        Err(VamanaError::InvalidConfig { .. })
    ));
}

#[test]
fn validate_rejects_alpha_below_one() {
    let cfg = VamanaConfig {
        alpha: 0.99,
        ..VamanaConfig::default()
    };
    assert!(matches!(
        cfg.validate(),
        Err(VamanaError::InvalidConfig { .. })
    ));
}

#[test]
#[should_panic(expected = "dimensions must be > 0")]
fn with_dimensions_panics_on_zero() {
    VamanaConfig::with_dimensions(0);
}

#[test]
fn builder_methods_set_fields() {
    let cfg = VamanaConfig::with_dimensions(128)
        .with_max_degree(32)
        .with_search_list_size(64)
        .with_alpha(1.5);
    assert_eq!(cfg.dimensions, 128);
    assert_eq!(cfg.max_degree, 32);
    assert_eq!(cfg.search_list_size, 64);
    assert_eq!(cfg.alpha, 1.5);
}
