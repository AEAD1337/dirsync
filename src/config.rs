use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Light,
    Dark,
    System,
}

/// Valid GUI port range, shared by every input path: `--port`, PUT /config,
/// and config.toml. Port 0 would bind an ephemeral port while the printed URL
/// and browser-open still say ":0"; ports below 1024 need elevation, and
/// browsers omit the default :80 from the Host header, which the same-origin
/// allowlist compares literally.
pub fn validate_port(port: u16) -> std::result::Result<(), String> {
    if port < 1024 {
        Err(format!("port {port} is invalid: must be 1024-65535"))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub port: u16,
    pub exclude_patterns: Vec<String>,
    pub last_src: Option<PathBuf>,
    pub last_dst: Option<PathBuf>,
    pub theme: Theme,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: 7373,
            exclude_patterns: Vec::new(),
            last_src: None,
            last_dst: None,
            theme: Theme::default(),
        }
    }
}

impl AppConfig {
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("dirsync").join("config.toml"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    pub(crate) fn load_from(path: &std::path::Path) -> Self {
        let mut cfg: Self = match std::fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        // A hand-edited port outside the valid range would bind an ephemeral
        // or privileged port that the printed URL and Host allowlist don't
        // match: fall back to the default like any other invalid config.
        if validate_port(cfg.port).is_err() {
            cfg.port = Self::default().port;
        }
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path().context("cannot determine config path")?;
        self.save_to(&path)
    }

    pub(crate) fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&tmp, contents)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn with_extra_excludes(mut self, extras: Vec<String>) -> Self {
        for e in extras {
            if !self.exclude_patterns.contains(&e) {
                self.exclude_patterns.push(e);
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_save_to_writes_valid_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let cfg = AppConfig {
            port: 9090,
            exclude_patterns: vec!["*.tmp".into()],
            last_src: None,
            last_dst: None,
            theme: Theme::Dark,
        };
        cfg.save_to(&path).unwrap();

        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: AppConfig = toml::from_str(&contents).unwrap();
        assert_eq!(parsed.port, 9090);
        assert_eq!(parsed.exclude_patterns, vec!["*.tmp"]);
        assert_eq!(parsed.theme, Theme::Dark);
    }

    #[test]
    fn test_save_to_atomic_leaves_no_tmp_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let tmp = dir.path().join("config.toml.tmp");

        AppConfig::default().save_to(&path).unwrap();

        assert!(path.exists());
        assert!(
            !tmp.exists(),
            ".tmp file should not remain after atomic rename"
        );
    }

    #[test]
    fn test_load_from_reads_back_saved_config() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let original = AppConfig {
            port: 8080,
            exclude_patterns: vec!["node_modules".into()],
            last_src: Some(PathBuf::from("/src")),
            last_dst: Some(PathBuf::from("/dst")),
            theme: Theme::System,
        };
        original.save_to(&path).unwrap();

        let loaded = AppConfig::load_from(&path);
        assert_eq!(loaded.port, 8080);
        assert_eq!(loaded.exclude_patterns, vec!["node_modules"]);
        assert_eq!(loaded.theme, Theme::System);
        assert_eq!(loaded.last_src, Some(PathBuf::from("/src")));
    }

    #[test]
    fn test_load_from_falls_back_on_missing_file() {
        let dir = TempDir::new().unwrap();
        let cfg = AppConfig::load_from(&dir.path().join("nonexistent.toml"));
        assert_eq!(cfg.port, 7373);
    }

    #[test]
    fn test_load_from_falls_back_on_invalid_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"port = [[[not valid toml").unwrap();

        let cfg = AppConfig::load_from(&path);
        assert_eq!(cfg.port, 7373);
    }

    #[test]
    fn test_save_to_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a").join("b").join("config.toml");

        AppConfig::default().save_to(&path).unwrap();
        assert!(path.exists());
    }
}
