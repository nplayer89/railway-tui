//! Listing a repo's branches through the GitHub CLI (`gh`), with the user's own
//! GitHub login: Railway refuses its GitHub queries to `railway login` tokens.

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// The `gh` program run by default (found on PATH).
pub const GH: &str = "gh";
const GH_TIMEOUT: Duration = Duration::from_secs(20);

/// Every branch name of `repo` (`owner/name`), following GitHub's pagination.
/// Errors are short and never hold more than the first line `gh` printed.
pub async fn branches(gh: &Path, repo: &str) -> Result<Vec<String>, String> {
    if !valid_repo(repo) {
        return Err(format!("not an owner/name repo: {repo}"));
    }
    let mut cmd = tokio::process::Command::new(gh);
    cmd.args(["api", &format!("repos/{repo}/branches"), "--paginate"])
        .args(["--jq", ".[].name"])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(GH_TIMEOUT, cmd.output()).await {
        Err(_) => return Err("gh timed out".into()),
        Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => {
            return Err("gh is not installed".into())
        }
        Ok(Err(e)) => return Err(format!("could not run gh: {}", e.kind())),
        Ok(Ok(output)) => output,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().map(str::trim).find(|l| !l.is_empty());
        return Err(format!("gh: {}", first.unwrap_or("failed")));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// `owner/name` made of the characters GitHub allows, so nothing else reaches the API path.
fn valid_repo(repo: &str) -> bool {
    let part = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    repo.split_once('/')
        .is_some_and(|(owner, name)| part(owner) && part(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in `gh` that prints `stdout`/`stderr` and exits with `code`.
    fn fake_gh(dir: &Path, stdout: &str, stderr: &str, code: i32) -> std::path::PathBuf {
        let path = dir.join("gh");
        let script = format!(
            "#!/bin/sh\necho \"$@\" > \"{}\"\nprintf '%s' '{stdout}'\nprintf '%s' '{stderr}' >&2\nexit {code}\n",
            dir.join("args").display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn lists_every_name_gh_prints() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "main\nfeat/x\n\n", "", 0);
        assert_eq!(branches(&gh, "acme/api").await.unwrap(), ["main", "feat/x"]);
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert_eq!(
            args.trim(),
            "api repos/acme/api/branches --paginate --jq .[].name"
        );
    }

    #[tokio::test]
    async fn a_failing_gh_reports_its_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "", "\nHTTP 404: Not Found\nmore", 1);
        assert_eq!(
            branches(&gh, "acme/api").await.unwrap_err(),
            "gh: HTTP 404: Not Found"
        );
    }

    #[tokio::test]
    async fn a_missing_gh_says_so() {
        let missing = Path::new("/nonexistent/gh");
        assert_eq!(
            branches(missing, "acme/api").await.unwrap_err(),
            "gh is not installed"
        );
    }

    #[test]
    fn only_owner_slash_name_is_accepted() {
        assert!(valid_repo("lttinc/novo-ui"));
        assert!(valid_repo("a_b/c.d"));
        for bad in [
            "noslash", "a/b/c", "/b", "a/", "../x", "a/..", "a/b?x=1", "-a/b c",
        ] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }
}
