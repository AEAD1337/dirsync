use dirsync::config::{AppConfig, Theme};

#[test]
fn test_default_config() {
    let cfg = AppConfig::default();
    assert_eq!(cfg.port, 7373);
    assert!(cfg.exclude_patterns.is_empty());
    assert!(cfg.last_src.is_none());
}

#[test]
fn test_toml_round_trip() {
    let cfg = AppConfig {
        port: 8080,
        exclude_patterns: vec!["*.tmp".into(), "node_modules".into()],
        last_src: Some("/tmp/src".into()),
        last_dst: Some("/tmp/dst".into()),
        theme: Theme::Dark,
    };
    let serialized = toml::to_string_pretty(&cfg).unwrap();
    let deserialized: AppConfig = toml::from_str(&serialized).unwrap();
    assert_eq!(deserialized.port, 8080);
    assert_eq!(deserialized.exclude_patterns, vec!["*.tmp", "node_modules"]);
    assert_eq!(deserialized.theme, Theme::Dark);
}

#[test]
fn test_with_extra_excludes() {
    let cfg = AppConfig {
        exclude_patterns: vec!["*.log".into()],
        ..Default::default()
    };
    let merged = cfg.with_extra_excludes(vec!["*.tmp".into(), "*.log".into()]);
    // *.log should not be duplicated
    assert_eq!(merged.exclude_patterns.len(), 2);
    assert!(merged.exclude_patterns.contains(&"*.tmp".into()));
}
