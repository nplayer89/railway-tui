//! Remembered selection, stored under the XDG state dir.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remembered {
    pub last_project_id: Option<String>,
    pub last_environment_id: Option<String>,
    /// Theme id chosen in the picker.
    pub theme: Option<String>,
}

/// `<xdg or home/.local/state>/railway-tui/state.toml`. An empty XDG value counts as unset.
pub fn remembered_path(xdg_state_home: Option<&Path>, home: &Path) -> PathBuf {
    let base = match xdg_state_home {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => home.join(".local").join("state"),
    };
    base.join("railway-tui").join("state.toml")
}

/// Never fails: a missing or corrupt file gives the default.
pub fn load_remembered(path: &Path) -> Remembered {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

/// Creates parent directories, writes a temp file, then renames it into place.
pub fn save_remembered(path: &Path, r: &Remembered) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(r).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn remembered_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/state.toml");
        let r = Remembered {
            last_project_id: Some("p1".into()),
            last_environment_id: Some("e1".into()),
            theme: Some("nord".into()),
        };
        save_remembered(&p, &r).unwrap();
        assert_eq!(load_remembered(&p), r);
    }

    #[test]
    fn corrupt_remembered_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.toml");
        std::fs::write(&p, "this is = = not toml [").unwrap();
        assert_eq!(load_remembered(&p), Remembered::default());
        assert_eq!(
            load_remembered(&dir.path().join("missing")),
            Remembered::default()
        );
    }

    #[test]
    fn remembered_path_defaults() {
        assert_eq!(
            remembered_path(None, Path::new("/home/u")),
            Path::new("/home/u/.local/state/railway-tui/state.toml")
        );
        assert_eq!(
            remembered_path(Some(Path::new("/s")), Path::new("/home/u")),
            Path::new("/s/railway-tui/state.toml")
        );
        assert_eq!(
            remembered_path(Some(Path::new("")), Path::new("/home/u")),
            Path::new("/home/u/.local/state/railway-tui/state.toml")
        );
    }
}
