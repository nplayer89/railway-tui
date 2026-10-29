//! Pending variable edits that have not been deployed yet, stored under the XDG state dir.
//! They hold secret values: the file is written 0600 and `Debug` never prints a value.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    Set { value: String },
    Delete,
}

impl fmt::Debug for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Set { .. } => f.write_str("Set { value: <redacted> }"),
            Change::Delete => f.write_str("Delete"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingVar {
    pub change: Change,
    /// Value on Railway when first edited; `None` = the variable was added.
    pub base: Option<String>,
    pub applied: bool,
}

impl fmt::Debug for PendingVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingVar")
            .field("change", &self.change)
            .field("base", &self.base.as_ref().map(|_| "<redacted>"))
            .field("applied", &self.applied)
            .finish()
    }
}

impl PendingVar {
    /// Railway now holds what this entry wrote: make that the base of a new,
    /// unapplied edit.
    fn rebase_on_written(&mut self) {
        self.base = match &self.change {
            Change::Set { value } => Some(value.clone()),
            Change::Delete => None,
        };
        self.applied = false;
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePending {
    pub project_id: String,
    pub env_name: String,
    pub service_name: String,
    pub vars: BTreeMap<String, PendingVar>,
    pub written_not_deployed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// environment id -> service id -> pending edits
    pub envs: BTreeMap<String, BTreeMap<String, ServicePending>>,
}

pub struct Target<'a> {
    pub project_id: &'a str,
    pub env_id: &'a str,
    pub env_name: &'a str,
    pub service_id: &'a str,
    pub service_name: &'a str,
}

impl Pending {
    fn entry(&mut self, t: &Target) -> &mut ServicePending {
        let sp = self
            .envs
            .entry(t.env_id.to_string())
            .or_default()
            .entry(t.service_id.to_string())
            .or_default();
        sp.project_id = t.project_id.to_string();
        sp.env_name = t.env_name.to_string();
        sp.service_name = t.service_name.to_string();
        sp
    }

    /// Drops an emptied group (and environment). A group whose remaining entries
    /// are all on Railway is written but not deployed.
    fn prune(&mut self, env_id: &str, service_id: &str) {
        if let Some(services) = self.envs.get_mut(env_id) {
            if let Some(sp) = services.get_mut(service_id) {
                if sp.vars.is_empty() {
                    services.remove(service_id);
                } else if sp.vars.values().all(|v| v.applied) {
                    sp.written_not_deployed = true;
                }
            }
            if services.is_empty() {
                self.envs.remove(env_id);
            }
        }
    }

    /// An entry already written to Railway (applied) is re-based on what it wrote,
    /// so going back to the old value is a real revert to send. Setting it to the
    /// value it already wrote changes nothing.
    pub fn set(&mut self, t: &Target, name: &str, value: String, current: Option<&str>) {
        let sp = self.entry(t);
        if let Some(v) = sp.vars.get_mut(name) {
            if v.applied {
                if matches!(&v.change, Change::Set { value: written } if *written == value) {
                    return;
                }
                v.rebase_on_written();
            }
        }
        let base = match sp.vars.get(name) {
            Some(v) => v.base.clone(),
            None => current.map(str::to_string),
        };
        if base.as_deref() == Some(value.as_str()) {
            sp.vars.remove(name);
        } else {
            sp.vars.insert(
                name.to_string(),
                PendingVar {
                    change: Change::Set { value },
                    base,
                    applied: false,
                },
            );
        }
        self.prune(t.env_id, t.service_id);
    }

    /// Like `set`: an applied entry is re-based on what it wrote first.
    pub fn delete(&mut self, t: &Target, name: &str, current: Option<&str>) {
        let sp = self.entry(t);
        if let Some(v) = sp.vars.get_mut(name) {
            if v.applied {
                if v.change == Change::Delete {
                    return;
                }
                v.rebase_on_written();
            }
        }
        let base = match sp.vars.get(name) {
            Some(v) => v.base.clone(),
            None => current.map(str::to_string),
        };
        if base.is_none() {
            sp.vars.remove(name);
        } else {
            sp.vars.insert(
                name.to_string(),
                PendingVar {
                    change: Change::Delete,
                    base,
                    applied: false,
                },
            );
        }
        self.prune(t.env_id, t.service_id);
    }

    /// Forgets an entry. The caller refuses this for an applied entry Neil asked to
    /// undo: Railway already holds its value.
    pub fn undo(&mut self, env_id: &str, service_id: &str, name: &str) {
        if let Some(sp) = self
            .envs
            .get_mut(env_id)
            .and_then(|s| s.get_mut(service_id))
        {
            sp.vars.remove(name);
        }
        self.prune(env_id, service_id);
    }

    pub fn discard(&mut self, env_id: &str, service_id: &str) {
        if let Some(services) = self.envs.get_mut(env_id) {
            services.remove(service_id);
        }
        self.prune(env_id, service_id);
    }

    pub fn service(&self, env_id: &str, service_id: &str) -> Option<&ServicePending> {
        self.envs.get(env_id)?.get(service_id)
    }

    pub fn count_for_project(&self, project_id: &str) -> usize {
        self.envs
            .values()
            .flat_map(|s| s.values())
            .filter(|s| s.project_id == project_id)
            .map(|s| s.vars.len())
            .sum()
    }
}

/// `<xdg or home/.local/state>/railway-tui/pending.toml`. An empty XDG value counts as unset.
pub fn pending_path(xdg_state_home: Option<&Path>, home: &Path) -> PathBuf {
    let base = match xdg_state_home {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => home.join(".local").join("state"),
    };
    base.join("railway-tui").join("pending.toml")
}

/// A missing file gives the default. A corrupt file is moved to `pending.toml.corrupt` and
/// reported, never silently dropped. Error text never includes file contents.
pub fn load_pending(path: &Path) -> Result<Pending, String> {
    let parsed = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str::<Pending>(&text).ok(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Pending::default()),
        // Not valid UTF-8: corrupt.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => None,
        Err(e) => {
            return Err(format!("could not read pending changes file: {}", e.kind()));
        }
    };
    if let Some(p) = parsed {
        return Ok(p);
    }
    let aside = path.with_extension("toml.corrupt");
    match std::fs::rename(path, &aside) {
        Ok(()) => Err(format!(
            "pending changes file was unreadable; moved to {}",
            aside.display()
        )),
        Err(e) => Err(format!(
            "pending changes file was unreadable and could not be moved aside: {}",
            e.kind()
        )),
    }
}

/// Creates parent directories, writes a 0600 temp file, then renames it into place.
pub fn save_pending(path: &Path, p: &Pending) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(p).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("toml.tmp");
    // A stale temp file would keep its old mode, so start from nothing.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn t<'a>(project: &'a str, env: &'a str, svc: &'a str) -> Target<'a> {
        Target {
            project_id: project,
            env_id: env,
            env_name: "production",
            service_id: svc,
            service_name: "api",
        }
    }

    #[test]
    fn set_records_base_once() {
        let mut p = Pending::default();
        let tg = t("p", "e", "s");
        p.set(&tg, "A", "two".into(), Some("one"));
        p.set(&tg, "A", "three".into(), Some("two"));
        let v = &p.service("e", "s").unwrap().vars["A"];
        assert_eq!(v.base.as_deref(), Some("one"));
        assert_eq!(
            v.change,
            Change::Set {
                value: "three".into()
            }
        );
    }

    #[test]
    fn set_back_to_base_removes() {
        let mut p = Pending::default();
        let tg = t("p", "e", "s");
        p.set(&tg, "A", "two".into(), Some("one"));
        p.set(&tg, "A", "one".into(), Some("two"));
        assert!(p.service("e", "s").is_none());
        assert!(p.envs.is_empty());
    }

    #[test]
    fn delete_added_removes() {
        let mut p = Pending::default();
        let tg = t("p", "e", "s");
        p.set(&tg, "NEW", "x".into(), None);
        assert_eq!(p.service("e", "s").unwrap().vars["NEW"].base, None);
        p.delete(&tg, "NEW", None);
        assert!(p.envs.is_empty());
    }

    #[test]
    fn delete_existing_records_base_and_refreshes_names() {
        let mut p = Pending::default();
        p.delete(&t("p", "e", "s"), "A", Some("one"));
        let tg = Target {
            project_id: "p2",
            env_id: "e",
            env_name: "staging",
            service_id: "s",
            service_name: "web",
        };
        p.set(&tg, "B", "x".into(), None);
        let sp = p.service("e", "s").unwrap();
        assert_eq!(sp.vars["A"].change, Change::Delete);
        assert_eq!(sp.vars["A"].base.as_deref(), Some("one"));
        assert_eq!(
            (&sp.project_id[..], &sp.env_name[..], &sp.service_name[..]),
            ("p2", "staging", "web")
        );
    }

    #[test]
    fn undo_and_discard_prune_empty_groups() {
        let mut p = Pending::default();
        let tg = t("p", "e", "s");
        p.set(&tg, "A", "2".into(), Some("1"));
        p.set(&tg, "B", "2".into(), Some("1"));
        p.undo("e", "s", "A");
        assert_eq!(p.service("e", "s").unwrap().vars.len(), 1);
        p.undo("e", "s", "B");
        assert!(p.envs.is_empty());
        p.set(&tg, "A", "2".into(), Some("1"));
        p.set(&t("p", "e", "s2"), "A", "2".into(), Some("1"));
        p.discard("e", "s");
        assert!(p.service("e", "s").is_none());
        assert!(p.service("e", "s2").is_some());
        p.discard("e", "s2");
        assert!(p.envs.is_empty());
    }

    /// Pending on (e, s): A set one → two and applied, NEW added as "n" and applied.
    fn applied_pending() -> Pending {
        let mut p = Pending::default();
        let tg = t("p", "e", "s");
        p.set(&tg, "A", "two".into(), Some("one"));
        p.set(&tg, "NEW", "n".into(), None);
        p.set(&tg, "B", "y".into(), Some("x"));
        let sp = p.envs.get_mut("e").unwrap().get_mut("s").unwrap();
        sp.vars.get_mut("A").unwrap().applied = true;
        sp.vars.get_mut("NEW").unwrap().applied = true;
        p
    }

    #[test]
    fn setting_an_applied_entry_back_is_a_revert_to_send() {
        let mut p = applied_pending();
        let tg = t("p", "e", "s");
        // Railway holds "two" now; going back to "one" must be written.
        p.set(&tg, "A", "one".into(), Some("two"));
        let v = &p.service("e", "s").unwrap().vars["A"];
        assert_eq!(
            v.change,
            Change::Set {
                value: "one".into()
            }
        );
        assert_eq!(v.base.as_deref(), Some("two"));
        assert!(!v.applied);
    }

    #[test]
    fn setting_an_applied_entry_to_its_written_value_changes_nothing() {
        let mut p = applied_pending();
        let before = p.clone();
        p.set(&t("p", "e", "s"), "A", "two".into(), Some("two"));
        assert_eq!(p, before);
        p.delete(&t("p", "e", "s"), "GONE", Some("g"));
        let sp = p.envs.get_mut("e").unwrap().get_mut("s").unwrap();
        sp.vars.get_mut("GONE").unwrap().applied = true;
        let before = p.clone();
        p.delete(&t("p", "e", "s"), "GONE", None);
        assert_eq!(p, before);
    }

    #[test]
    fn deleting_an_applied_addition_deletes_it_on_railway() {
        let mut p = applied_pending();
        p.delete(&t("p", "e", "s"), "NEW", Some("n"));
        let v = &p.service("e", "s").unwrap().vars["NEW"];
        assert_eq!(v.change, Change::Delete);
        assert_eq!(v.base.as_deref(), Some("n"));
        assert!(!v.applied);
    }

    #[test]
    fn editing_an_applied_delete_is_an_addition() {
        let mut p = Pending::default();
        let tg = t("p", "e", "s");
        p.delete(&tg, "GONE", Some("g"));
        let sp = p.envs.get_mut("e").unwrap().get_mut("s").unwrap();
        sp.vars.get_mut("GONE").unwrap().applied = true;
        sp.written_not_deployed = true;
        p.set(&tg, "GONE", "back".into(), None);
        let sp = p.service("e", "s").unwrap();
        let v = &sp.vars["GONE"];
        assert_eq!(
            v.change,
            Change::Set {
                value: "back".into()
            }
        );
        assert_eq!(v.base, None);
        assert!(!v.applied);
        assert!(sp.written_not_deployed, "the written state is kept");
    }

    #[test]
    fn dropping_the_last_unapplied_entry_leaves_the_group_written() {
        let mut p = applied_pending();
        let tg = t("p", "e", "s");
        assert!(!p.service("e", "s").unwrap().written_not_deployed);
        // B goes back to its base: only applied entries remain.
        p.set(&tg, "B", "x".into(), Some("x"));
        let sp = p.service("e", "s").unwrap();
        assert_eq!(sp.vars.len(), 2);
        assert!(sp.written_not_deployed);
    }

    #[test]
    fn count_for_project_counts_vars_in_that_project_only() {
        let mut p = Pending::default();
        p.set(&t("p1", "e1", "s1"), "A", "2".into(), Some("1"));
        p.set(&t("p1", "e1", "s1"), "B", "2".into(), Some("1"));
        p.set(&t("p1", "e2", "s2"), "A", "2".into(), Some("1"));
        p.set(&t("p2", "e3", "s3"), "A", "2".into(), Some("1"));
        assert_eq!(p.count_for_project("p1"), 3);
        assert_eq!(p.count_for_project("p2"), 1);
        assert_eq!(p.count_for_project("nope"), 0);
    }

    #[test]
    fn roundtrip_preserves_odd_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/pending.toml");
        let mut p = Pending::default();
        let tg = t("p", "e", "s");
        let odd = [
            ("EMPTY", String::new()),
            ("EQ", "a=b".into()),
            ("Q", "\"x'".into()),
            ("NL", "l1\nl2".into()),
            ("U", "héllo ✓".into()),
            ("BIG", "x".repeat(10240)),
        ];
        for (k, v) in odd {
            p.set(&tg, k, v, Some("old"));
        }
        p.delete(&tg, "GONE", Some("a\nb"));
        p.envs
            .get_mut("e")
            .unwrap()
            .get_mut("s")
            .unwrap()
            .written_not_deployed = true;
        save_pending(&path, &p).unwrap();
        assert_eq!(load_pending(&path).unwrap(), p);
    }

    #[test]
    fn save_is_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.toml");
        // A stale, world-readable temp file must not leak its mode.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, "x").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_pending(&path, &Pending::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_pending(&dir.path().join("none.toml")).unwrap(),
            Pending::default()
        );
    }

    #[test]
    fn corrupt_file_is_moved_aside_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.toml");
        std::fs::write(&path, "SECRET = = not toml [").unwrap();
        let err = load_pending(&path).unwrap_err();
        let aside = dir.path().join("pending.toml.corrupt");
        assert!(err.starts_with("pending changes file was unreadable; moved to "));
        assert!(err.contains(&aside.display().to_string()));
        assert!(!err.contains("SECRET"));
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "SECRET = = not toml ["
        );
    }

    #[test]
    fn debug_redacts_values() {
        let mut p = Pending::default();
        let tg = t("p", "e", "s");
        p.set(&tg, "A", "newsecret".into(), Some("oldsecret"));
        p.set(&tg, "B", "other".into(), None);
        p.delete(&tg, "C", Some("deletedsecret"));
        let out = format!("{p:?}");
        for v in ["newsecret", "oldsecret", "other", "deletedsecret"] {
            assert!(!out.contains(v), "{v} leaked: {out}");
        }
        assert!(out.contains("<redacted>"));
        assert!(out.contains("\"A\""));
    }

    #[test]
    fn pending_path_defaults() {
        assert_eq!(
            pending_path(None, Path::new("/home/u")),
            Path::new("/home/u/.local/state/railway-tui/pending.toml")
        );
        assert_eq!(
            pending_path(Some(Path::new("/s")), Path::new("/home/u")),
            Path::new("/s/railway-tui/pending.toml")
        );
    }
}
