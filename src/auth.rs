//! Auth token sources. The TUI never refreshes OAuth tokens itself: Railway's
//! refresh tokens rotate, so a concurrent refresh would revoke the user's whole
//! login. Instead we read the `railway` CLI's config file (read-only) and, when
//! the token is stale, let the CLI refresh it by running `railway whoami`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Tokens expiring within this many seconds are treated as already expired.
const EXPIRY_SKEW_SECS: i64 = 60;
const WHOAMI_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Not logged in to Railway (no token in {}). Run `railway login`.", path.display())]
    NotLoggedIn { path: PathBuf },
    #[error("The `railway` CLI was not found on PATH. Install it, or set RAILWAY_API_TOKEN.")]
    CliMissing,
    #[error("Could not refresh the Railway login: {0}. Run `railway login`.")]
    RefreshFailed(String),
    #[error("Railway rejected the API token. Check RAILWAY_API_TOKEN, or run `railway login`.")]
    Rejected,
    #[error("Auth I/O error: {0}")]
    Io(String),
}

pub trait TokenSource: Send + Sync {
    fn token(&self) -> Result<String, AuthError>;
    /// Called after the API returned 401: obtain a fresh token if possible.
    fn refresh_after_rejection(&self) -> Result<String, AuthError>;
}

/// A static token from `RAILWAY_API_TOKEN`; it cannot be refreshed.
pub struct EnvTokenSource(pub String);

impl TokenSource for EnvTokenSource {
    fn token(&self) -> Result<String, AuthError> {
        Ok(self.0.clone())
    }
    fn refresh_after_rejection(&self) -> Result<String, AuthError> {
        Err(AuthError::Rejected)
    }
}

pub trait CliRunner: Send + Sync {
    /// Run `railway whoami`, which makes the CLI refresh its own config.
    fn run_whoami(&self) -> Result<(), AuthError>;
}

pub struct RealCliRunner;

impl CliRunner for RealCliRunner {
    fn run_whoami(&self) -> Result<(), AuthError> {
        let mut child = Command::new("railway")
            .arg("whoami")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => AuthError::CliMissing,
                _ => AuthError::Io(e.to_string()),
            })?;
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    return Err(AuthError::RefreshFailed(format!(
                        "`railway whoami` exited with {status}"
                    )))
                }
                Ok(None) if start.elapsed() >= WHOAMI_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AuthError::RefreshFailed(
                        "`railway whoami` timed out".into(),
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(AuthError::Io(e.to_string())),
            }
        }
    }
}

#[derive(Deserialize)]
struct ConfigFile {
    user: Option<UserSection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserSection {
    token: Option<String>,
    access_token: Option<String>,
    token_expires_at: Option<i64>,
}

struct Creds {
    token: String,
    expires_at: Option<i64>,
}

pub struct CliTokenSource<R: CliRunner> {
    config_path: PathBuf,
    runner: R,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    refresh_lock: Mutex<()>,
}

impl<R: CliRunner> CliTokenSource<R> {
    pub fn new(config_path: PathBuf, runner: R, now: Box<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            config_path,
            runner,
            now,
            refresh_lock: Mutex::new(()),
        }
    }

    fn not_logged_in(&self) -> AuthError {
        AuthError::NotLoggedIn {
            path: self.config_path.clone(),
        }
    }

    /// Reads the CLI config from disk (never cached, never written).
    fn read_creds(&self) -> Result<Creds, AuthError> {
        let bytes = match std::fs::read(&self.config_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(self.not_logged_in()),
            Err(e) => {
                return Err(AuthError::Io(format!(
                    "{}: {e}",
                    self.config_path.display()
                )))
            }
        };
        let file: ConfigFile = serde_json::from_slice(&bytes)
            .map_err(|e| AuthError::Io(format!("{}: {e}", self.config_path.display())))?;
        let user = file.user.ok_or_else(|| self.not_logged_in())?;
        let non_empty = |s: Option<String>| s.filter(|s| !s.is_empty());
        if let Some(token) = non_empty(user.access_token) {
            Ok(Creds {
                token,
                expires_at: user.token_expires_at,
            })
        } else if let Some(token) = non_empty(user.token) {
            Ok(Creds {
                token,
                expires_at: None,
            })
        } else {
            Err(self.not_logged_in())
        }
    }

    fn is_expired(&self, creds: &Creds) -> bool {
        creds
            .expires_at
            .is_some_and(|exp| exp <= (self.now)() + EXPIRY_SKEW_SECS)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.refresh_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Caller must hold `refresh_lock`.
    fn whoami_and_reread(&self) -> Result<String, AuthError> {
        self.runner.run_whoami()?;
        // The CLI may have been mid-write; retry a failed read once.
        let creds = match self.read_creds() {
            Err(AuthError::Io(_)) => {
                std::thread::sleep(Duration::from_millis(100));
                self.read_creds()?
            }
            other => other?,
        };
        if self.is_expired(&creds) {
            return Err(AuthError::RefreshFailed(
                "token still expired after `railway whoami`".into(),
            ));
        }
        Ok(creds.token)
    }
}

impl<R: CliRunner> TokenSource for CliTokenSource<R> {
    fn token(&self) -> Result<String, AuthError> {
        let creds = self.read_creds()?;
        if !self.is_expired(&creds) {
            return Ok(creds.token);
        }
        let _guard = self.lock();
        // Another thread may have refreshed while we waited for the lock.
        let creds = self.read_creds()?;
        if !self.is_expired(&creds) {
            return Ok(creds.token);
        }
        self.whoami_and_reread()
    }

    fn refresh_after_rejection(&self) -> Result<String, AuthError> {
        // The token the caller just had rejected.
        let rejected = self.read_creds().ok().map(|c| c.token);
        let _guard = self.lock();
        // Another caller may have refreshed while we waited for the lock.
        if let Ok(creds) = self.read_creds() {
            if Some(&creds.token) != rejected.as_ref() && !self.is_expired(&creds) {
                return Ok(creds.token);
            }
        }
        let token = self.whoami_and_reread()?;
        if Some(&token) == rejected.as_ref() {
            // The CLI only refreshes expired tokens, so this one was revoked server-side.
            return Err(AuthError::Rejected);
        }
        Ok(token)
    }
}

/// Path of the CLI's config file for the given `RAILWAY_ENV`.
pub fn cli_config_path(railway_env: Option<&str>, home: &Path) -> PathBuf {
    let name = match railway_env {
        Some("staging") => "config-staging.json",
        Some("dev") | Some("develop") => "config-dev.json",
        _ => "config.json",
    };
    home.join(".railway").join(name)
}

/// The only function that reads the real environment.
pub fn token_source_from_env() -> Result<Arc<dyn TokenSource>, AuthError> {
    if let Ok(token) = std::env::var("RAILWAY_API_TOKEN") {
        if !token.is_empty() {
            return Ok(Arc::new(EnvTokenSource(token)));
        }
    }
    let home =
        dirs::home_dir().ok_or_else(|| AuthError::Io("cannot determine home directory".into()))?;
    let railway_env = std::env::var("RAILWAY_ENV").ok();
    let path = cli_config_path(railway_env.as_deref(), &home);
    let now = Box::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });
    Ok(Arc::new(CliTokenSource::new(path, RealCliRunner, now)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const NOW: i64 = 1_000_000;

    struct FakeRunner {
        calls: Arc<AtomicUsize>,
        path: PathBuf,
        rewrite: Option<String>,
        cli_missing: bool,
        delay_ms: u64,
    }

    impl CliRunner for FakeRunner {
        fn run_whoami(&self) -> Result<(), AuthError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.cli_missing {
                return Err(AuthError::CliMissing);
            }
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
            if let Some(content) = &self.rewrite {
                std::fs::write(&self.path, content).unwrap();
            }
            Ok(())
        }
    }

    fn cfg(access: Option<&str>, expires: Option<i64>, legacy: Option<&str>) -> String {
        let mut user = serde_json::Map::new();
        if let Some(a) = access {
            user.insert("accessToken".into(), a.into());
        }
        if let Some(e) = expires {
            user.insert("tokenExpiresAt".into(), e.into());
        }
        if let Some(l) = legacy {
            user.insert("token".into(), l.into());
        }
        user.insert("refreshToken".into(), "r".into());
        serde_json::json!({ "user": user, "projects": {} }).to_string()
    }

    fn source(
        content: Option<&str>,
        rewrite: Option<String>,
        cli_missing: bool,
    ) -> (
        tempfile::TempDir,
        CliTokenSource<FakeRunner>,
        Arc<AtomicUsize>,
    ) {
        source_delayed(content, rewrite, cli_missing, 0)
    }

    fn source_delayed(
        content: Option<&str>,
        rewrite: Option<String>,
        cli_missing: bool,
        delay_ms: u64,
    ) -> (
        tempfile::TempDir,
        CliTokenSource<FakeRunner>,
        Arc<AtomicUsize>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        if let Some(c) = content {
            std::fs::write(&path, c).unwrap();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = FakeRunner {
            calls: calls.clone(),
            path: path.clone(),
            rewrite,
            cli_missing,
            delay_ms,
        };
        let src = CliTokenSource::new(path, runner, Box::new(|| NOW));
        (dir, src, calls)
    }

    #[test]
    fn uses_access_token_when_fresh() {
        let (_d, s, calls) = source(Some(&cfg(Some("acc"), Some(NOW + 3600), None)), None, false);
        assert_eq!(s.token().unwrap(), "acc");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn expired_token_runs_whoami_then_rereads() {
        let new = cfg(Some("new"), Some(NOW + 3600), None);
        let (_d, s, calls) = source(
            Some(&cfg(Some("old"), Some(NOW - 10), None)),
            Some(new),
            false,
        );
        assert_eq!(s.token().unwrap(), "new");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn within_60s_counts_as_expired() {
        let new = cfg(Some("new"), Some(NOW + 3600), None);
        let (_d, s, calls) = source(
            Some(&cfg(Some("old"), Some(NOW + 60), None)),
            Some(new),
            false,
        );
        assert_eq!(s.token().unwrap(), "new");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn still_expired_after_whoami_is_refresh_failed() {
        let (_d, s, calls) = source(Some(&cfg(Some("old"), Some(NOW - 10), None)), None, false);
        assert!(matches!(s.token(), Err(AuthError::RefreshFailed(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn legacy_token_used_when_no_access_token() {
        let (_d, s, calls) = source(Some(&cfg(None, None, Some("legacy"))), None, false);
        assert_eq!(s.token().unwrap(), "legacy");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn missing_file_or_user_is_not_logged_in() {
        let (_d, s, _) = source(None, None, false);
        let err = s.token().unwrap_err();
        assert!(matches!(err, AuthError::NotLoggedIn { .. }));
        assert!(err.to_string().contains("railway login"));

        let (_d, s, _) = source(Some(r#"{"projects":{}}"#), None, false);
        let err = s.token().unwrap_err();
        assert!(matches!(err, AuthError::NotLoggedIn { .. }));
        assert!(err.to_string().contains("railway login"));

        let (_d, s, _) = source(Some(r#"{"user":{"refreshToken":"r"}}"#), None, false);
        assert!(matches!(s.token(), Err(AuthError::NotLoggedIn { .. })));
    }

    #[test]
    fn cli_missing_surfaces() {
        let (_d, s, _) = source(Some(&cfg(Some("old"), Some(NOW - 10), None)), None, true);
        assert!(matches!(s.token(), Err(AuthError::CliMissing)));
    }

    #[test]
    fn never_writes_config() {
        let content = cfg(Some("acc"), Some(NOW - 10), None);
        let (d, s, _) = source(Some(&content), None, false);
        let _ = s.token();
        let _ = s.refresh_after_rejection();
        let after = std::fs::read(d.path().join("config.json")).unwrap();
        assert_eq!(after, content.as_bytes());
    }

    #[test]
    fn refresh_after_rejection_runs_whoami_once_and_rereads() {
        let new = cfg(Some("new"), Some(NOW + 3600), None);
        let (_d, s, calls) = source(
            Some(&cfg(Some("old"), Some(NOW + 3600), None)),
            Some(new),
            false,
        );
        assert_eq!(s.refresh_after_rejection().unwrap(), "new");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_rejections_run_whoami_once() {
        let new = cfg(Some("new"), Some(NOW + 3600), None);
        let (_d, s, calls) = source_delayed(
            Some(&cfg(Some("old"), Some(NOW + 3600), None)),
            Some(new),
            false,
            300,
        );
        let s = Arc::new(s);
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || s.refresh_after_rejection())
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap().unwrap(), "new");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_expired_tokens_run_whoami_once() {
        let new = cfg(Some("new"), Some(NOW + 3600), None);
        let (_d, s, calls) = source_delayed(
            Some(&cfg(Some("old"), Some(NOW - 10), None)),
            Some(new),
            false,
            300,
        );
        let s = Arc::new(s);
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || s.token())
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap().unwrap(), "new");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unchanged_token_after_rejection_is_rejected() {
        let (_d, s, calls) = source(Some(&cfg(Some("acc"), Some(NOW + 3600), None)), None, false);
        let err = s.refresh_after_rejection().unwrap_err();
        assert!(matches!(err, AuthError::Rejected));
        assert!(err.to_string().contains("railway login"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn env_source_cannot_refresh() {
        let s = EnvTokenSource("t".into());
        assert_eq!(s.token().unwrap(), "t");
        assert!(matches!(
            s.refresh_after_rejection(),
            Err(AuthError::Rejected)
        ));
    }

    #[test]
    fn cli_config_path_variants() {
        let h = Path::new("/h");
        assert_eq!(
            cli_config_path(Some("staging"), h),
            Path::new("/h/.railway/config-staging.json")
        );
        assert_eq!(
            cli_config_path(Some("dev"), h),
            Path::new("/h/.railway/config-dev.json")
        );
        assert_eq!(
            cli_config_path(Some("develop"), h),
            Path::new("/h/.railway/config-dev.json")
        );
        assert_eq!(
            cli_config_path(Some("production"), h),
            Path::new("/h/.railway/config.json")
        );
        assert_eq!(
            cli_config_path(None, h),
            Path::new("/h/.railway/config.json")
        );
    }
}
