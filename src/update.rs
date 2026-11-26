//! "A new version is available" check against the project's GitHub Releases.
//!
//! The repository is taken from `FOXGIT_REPO` or, in GitHub Actions release builds, from the
//! automatic `GITHUB_REPOSITORY` variable — so local/dev builds simply don't check.

use serde::Deserialize;
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `owner/name` of the repository releases are published to.
pub const REPO: Option<&str> = match option_env!("FOXGIT_REPO") {
    Some(r) => Some(r),
    None => option_env!("GITHUB_REPOSITORY"),
};

pub fn repo_url() -> Option<String> {
    REPO.map(|r| format!("https://github.com/{r}"))
}

#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
}

impl Release {
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches(['v', 'V'])
    }
}

/// What a finished update check sends back: (asked from the menu, result).
pub type CheckResult = (bool, Result<Option<Release>, String>);

/// The newest published release, if it is newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    let Some(repo) = REPO else {
        return Err("This build doesn't know where its releases are published".into());
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .user_agent(format!("FoxGit/{VERSION}"))
        .build()
        .into();
    let mut r = agent
        .get(&format!(
            "https://api.github.com/repos/{repo}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("Network error: {e}"))?;
    match r.status().as_u16() {
        200 => {}
        404 => return Ok(None), // no release yet
        s => return Err(format!("GitHub returned HTTP {s}")),
    }
    let rel: Release = r.body_mut().read_json().map_err(|e| e.to_string())?;
    Ok((!rel.draft && !rel.prerelease && is_newer(rel.version(), VERSION)).then_some(rel))
}

/// Compare dotted versions numerically ("0.10.0" > "0.9.3"); pre-release suffixes are ignored.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    /// Talks to GitHub: `FOXGIT_REPO=BurntSushi/ripgrep cargo test live_check -- --ignored`
    #[test]
    #[ignore]
    fn live_check() {
        let rel = super::check()
            .expect("check failed")
            .expect("expected a newer release");
        assert!(rel.html_url.starts_with("https://github.com/"), "{rel:?}");
    }

    #[test]
    fn version_order() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.3"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.1.0-beta", "0.1.0"));
    }
}
