//! User config file (`railway-tui.conf`, TOML).

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Settings from the config file. Defaults match a workspace, project or
/// environment by id, or by name case-insensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub rate_limit_per_hour: Option<u32>,
    pub refresh_interval_secs: Option<u64>,
    pub default_workspace: Option<String>,
    pub default_project: Option<String>,
    pub default_environment: Option<String>,
    /// Theme id or name; a theme picked in the app takes precedence.
    pub theme: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// `<xdg or home/.config>/railway-tui/railway-tui.conf`. An empty XDG value counts as unset.
pub fn config_path(xdg_config_home: Option<&Path>, home: &Path) -> PathBuf {
    let base = match xdg_config_home {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => home.join(".config"),
    };
    base.join("railway-tui").join("railway-tui.conf")
}

/// A missing file gives the default config.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let p = dir.path().join("railway-tui.conf");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn missing_config_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let c = load_config(&dir.path().join("nope.conf")).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn full_config_parses() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "rate_limit_per_hour = 1000\nrefresh_interval_secs = 30\ndefault_project = \"my-api\"\n",
        );
        let c = load_config(&p).unwrap();
        assert_eq!(c.rate_limit_per_hour, Some(1000));
        assert_eq!(c.refresh_interval_secs, Some(30));
        assert_eq!(c.default_project.as_deref(), Some("my-api"));
        assert_eq!(c.default_workspace, None);
    }

    #[test]
    fn unknown_key_errors_naming_file_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "rate_limit = 5\n");
        let msg = load_config(&p).unwrap_err().to_string();
        assert!(msg.contains(&p.display().to_string()), "{msg}");
        assert!(msg.contains("rate_limit"), "{msg}");
    }

    #[test]
    fn wrong_type_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "rate_limit_per_hour = \"lots\"\n");
        let msg = load_config(&p).unwrap_err().to_string();
        assert!(msg.contains(&p.display().to_string()), "{msg}");
    }

    #[test]
    fn config_path_respects_xdg() {
        let p = config_path(Some(Path::new("/x/cfg")), Path::new("/home/u"));
        assert_eq!(p, Path::new("/x/cfg/railway-tui/railway-tui.conf"));
    }

    #[test]
    fn config_path_defaults_to_home_dot_config() {
        let p = config_path(None, Path::new("/home/u"));
        assert_eq!(p, Path::new("/home/u/.config/railway-tui/railway-tui.conf"));
        let p = config_path(Some(Path::new("")), Path::new("/home/u"));
        assert_eq!(p, Path::new("/home/u/.config/railway-tui/railway-tui.conf"));
    }
}
