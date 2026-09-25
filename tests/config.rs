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
        ..Default::default()
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

// --- Loading: an explicit path is a promise, the default path is a hint ---

#[test]
fn load_explicit_rejects_a_missing_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("jbo.toml");

    // A typo in --config must not silently drop the job's excludes: the
    // content they protect in DST would be planned for deletion.
    let err = AppConfig::load_explicit(&path).unwrap_err();

    assert!(err.to_string().contains("jbo.toml"), "{err}");
}

#[test]
fn load_explicit_rejects_an_unparseable_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("job.toml");
    std::fs::write(&path, "exclude_patterns = [[[").unwrap();

    assert!(AppConfig::load_explicit(&path).is_err());
}

#[test]
fn load_explicit_reads_a_valid_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("job.toml");
    std::fs::write(&path, "exclude_patterns = [\".git\"]\n").unwrap();

    let cfg = AppConfig::load_explicit(&path).unwrap();

    assert_eq!(cfg.exclude_patterns, vec![".git"]);
    assert_eq!(cfg.path.as_deref(), Some(path.as_path()));
}

fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[test]
fn load_decodes_a_utf16_file_written_by_windows_powershell() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    // What `'...' > config.toml` produces in Windows PowerShell 5.1.
    std::fs::write(&path, utf16le_with_bom("exclude_patterns = [\".git\"]\r\n")).unwrap();

    assert_eq!(AppConfig::load_from(&path).exclude_patterns, vec![".git"]);
    assert_eq!(
        AppConfig::load_explicit(&path).unwrap().exclude_patterns,
        vec![".git"]
    );
}

#[test]
fn load_accepts_a_utf8_bom() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"port = 8080\n");
    std::fs::write(&path, bytes).unwrap();

    assert_eq!(AppConfig::load_from(&path).port, 8080);
}

#[test]
fn load_from_backs_up_undecodable_bytes_before_falling_back() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    let raw = vec![b'p', 0xC3, 0x28, 0xFF, b'\n'];
    std::fs::write(&path, &raw).unwrap();

    let cfg = AppConfig::load_from(&path);

    assert_eq!(cfg.port, 7373);
    // The next save() overwrites the file: the original bytes must survive.
    assert_eq!(
        std::fs::read(dir.path().join("config.toml.bad")).unwrap(),
        raw
    );
}

// --- Saving: session-only overrides never reach the file ---

#[test]
fn cli_excludes_are_applied_but_never_saved() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "exclude_patterns = [\"*.log\"]\n").unwrap();

    let cfg =
        AppConfig::load_from(&path).with_extra_excludes(vec!["secret".into(), "*.log".into()]);
    assert_eq!(cfg.exclude_patterns, vec!["*.log", "secret"]);
    cfg.save().unwrap();

    assert_eq!(AppConfig::load_from(&path).exclude_patterns, vec!["*.log"]);
}

#[test]
fn concurrent_saves_to_one_path_all_succeed() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    let cfg = AppConfig::load_from(&path);

    // Two writers (PUT /config and a preview) used to share one .tmp name,
    // so the losing rename failed with NotFound on Windows.
    let failures: usize = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cfg = cfg.clone();
                s.spawn(move || (0..25).filter(|_| cfg.save().is_err()).count())
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).sum()
    });

    assert_eq!(failures, 0);
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != "config.toml")
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
