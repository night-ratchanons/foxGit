//! GitHub backend: personal-access-token sign-in, token storage in the OS keychain, listing the
//! repositories the account can access, and handing the token to git for HTTPS remotes on
//! github.com.
//!
//! All functions here block on the network — call them from a background thread.

use serde::Deserialize;
use std::sync::Mutex;
use std::time::Duration;

const API: &str = "https://api.github.com";
const KEYCHAIN_SERVICE: &str = "FoxGit GitHub";
/// Where tokens were kept before the app was renamed (moved over on first read).
const LEGACY_KEYCHAIN_SERVICE: &str = "RustTree GitHub";
/// Classic token with the scopes FoxGit needs pre-ticked: `repo` (private repositories),
/// `read:org` (organization repositories) and `workflow` (pushes that touch .github/workflows).
pub const NEW_TOKEN_URL: &str =
    "https://github.com/settings/tokens/new?scopes=repo,read:org,workflow&description=FoxGit";
pub const NEW_FINE_GRAINED_TOKEN_URL: &str =
    "https://github.com/settings/personal-access-tokens/new";

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub html_url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GhRepo {
    pub full_name: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub archived: bool,
    pub clone_url: String,
    pub ssh_url: String,
    pub html_url: String,
}

#[derive(Debug)]
pub enum ApiError {
    /// The token was rejected (expired or revoked).
    Auth(String),
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Auth(m) | ApiError::Other(m) => f.write_str(m),
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .user_agent("FoxGit")
        .build()
        .into()
}

fn net(e: ureq::Error) -> ApiError {
    ApiError::Other(format!("Network error: {e}"))
}

fn api_get(token: &str, path: &str) -> Result<String, ApiError> {
    let mut r = agent()
        .get(&format!("{API}{path}"))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(net)?;
    let status = r.status().as_u16();
    let body = r
        .body_mut()
        .read_to_string()
        .map_err(|e| ApiError::Other(e.to_string()))?;
    if (200..300).contains(&status) {
        return Ok(body);
    }
    let msg = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["message"].as_str().map(String::from))
        .unwrap_or_else(|| body.chars().take(200).collect());
    Err(match status {
        401 => ApiError::Auth(
            "GitHub rejected the token (401) — it may have expired or been revoked".into(),
        ),
        _ => ApiError::Other(format!("GitHub API error {status}: {msg}")),
    })
}

/// The account a token belongs to.
pub fn fetch_user(token: &str) -> Result<User, ApiError> {
    let body = api_get(token, "/user")?;
    serde_json::from_str(&body).map_err(|e| ApiError::Other(e.to_string()))
}

/// Every repository the account can access: its own, ones it collaborates on, and those of
/// organizations it belongs to.
pub fn list_repos(token: &str) -> Result<Vec<GhRepo>, ApiError> {
    let mut all: Vec<GhRepo> = Vec::new();
    for page in 1..=50 {
        let body = api_get(
            token,
            &format!(
                "/user/repos?per_page=100&page={page}&sort=full_name\
                 &affiliation=owner,collaborator,organization_member"
            ),
        )?;
        let batch: Vec<GhRepo> =
            serde_json::from_str(&body).map_err(|e| ApiError::Other(e.to_string()))?;
        let n = batch.len();
        all.extend(batch);
        if n < 100 {
            break;
        }
    }
    all.sort_by_key(|r| r.full_name.to_lowercase());
    all.dedup_by(|a, b| a.full_name == b.full_name);
    Ok(all)
}

// ---------------------------------------------------------------------------
// Keychain
// ---------------------------------------------------------------------------

pub fn save_token(login: &str, token: &str) -> Result<(), String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, login)
        .and_then(|e| e.set_password(token))
        .map_err(|e| e.to_string())
}

pub fn load_token(login: &str) -> Result<String, String> {
    let current = keyring::Entry::new(KEYCHAIN_SERVICE, login).and_then(|e| e.get_password());
    match current {
        Ok(t) => Ok(t),
        Err(err) => {
            // Signed in before the rename? Move the token to the new keychain entry.
            let legacy = keyring::Entry::new(LEGACY_KEYCHAIN_SERVICE, login)
                .and_then(|e| e.get_password().map(|t| (e, t)));
            match legacy {
                Ok((old, t)) => {
                    if save_token(login, &t).is_ok() {
                        let _ = old.delete_credential();
                    }
                    Ok(t)
                }
                Err(_) => Err(err.to_string()),
            }
        }
    }
}

pub fn delete_token(login: &str) {
    if let Ok(e) = keyring::Entry::new(KEYCHAIN_SERVICE, login) {
        let _ = e.delete_credential();
    }
}

// ---------------------------------------------------------------------------
// Credentials for git
// ---------------------------------------------------------------------------

static GIT_CREDENTIAL: Mutex<Option<(String, String)>> = Mutex::new(None);
const ENV_USER: &str = "FOXGIT_GH_USER";
const ENV_TOKEN: &str = "FOXGIT_GH_TOKEN";

/// Use this account (login, token) for every HTTPS remote on github.com, or stop doing so.
pub fn set_git_credential(cred: Option<(String, String)>) {
    if let Ok(mut c) = GIT_CREDENTIAL.lock() {
        *c = cred;
    }
}

/// `git -c …` arguments and environment variables for a git child process.
pub type GitCredential = (Vec<String>, Vec<(&'static str, String)>);

/// Extra `git -c …` arguments and environment for a git child process.
///
/// The empty `helper=` resets the helper list for github.com only (so a stale entry in
/// osxkeychain / Credential Manager can't win), then a tiny inline helper answers with the
/// token from the environment. Other hosts keep the user's own helpers.
pub fn git_credential() -> Option<GitCredential> {
    let c = GIT_CREDENTIAL.lock().ok()?;
    let (user, token) = c.as_ref()?;
    let helper = format!(
        "!f() {{ test \"$1\" = get || exit 0; echo \"username=${ENV_USER}\"; echo \"password=${ENV_TOKEN}\"; }}; f"
    );
    Some((
        vec![
            "-c".into(),
            "credential.https://github.com.helper=".into(),
            "-c".into(),
            format!("credential.https://github.com.helper={helper}"),
        ],
        vec![(ENV_USER, user.clone()), (ENV_TOKEN, token.clone())],
    ))
}

/// `owner/name` (lower-case) for a github.com remote URL in any of its usual forms.
pub fn repo_slug(url: &str) -> Option<String> {
    let lower = url.trim().to_lowercase();
    let at = lower.find("github.com")?;
    let rest = lower[at + "github.com".len()..].trim_start_matches([':', '/']);
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        for u in [
            "https://github.com/Owner/Repo.git",
            "https://github.com/owner/repo",
            "https://user@github.com/owner/repo.git/",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo",
        ] {
            assert_eq!(repo_slug(u).as_deref(), Some("owner/repo"), "{u}");
        }
        assert_eq!(repo_slug("https://gitlab.com/owner/repo"), None);
        assert_eq!(repo_slug("https://github.com/owner"), None);
    }

    #[test]
    fn git_uses_token_for_github_only() {
        set_git_credential(Some(("octo".into(), "tok123".into())));
        let (args, env) = git_credential().unwrap();
        set_git_credential(None);
        let dir = tempfile::tempdir().unwrap();
        let fill = |host: &str| {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(&args)
                .args([
                    "-c",
                    "credential.helper=!f() { echo username=other; echo password=otherpw; }; f",
                ])
                .args(["credential", "fill"])
                .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
                .env("GIT_TERMINAL_PROMPT", "0")
                // keep the user's own helpers (osxkeychain …) out of the test
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut c| {
                    use std::io::Write;
                    c.stdin
                        .take()
                        .unwrap()
                        .write_all(format!("protocol=https\nhost={host}\n\n").as_bytes())?;
                    c.wait_with_output()
                })
                .unwrap();
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        // The generic helper is configured after ours here, so it is in the list for github.com
        // too — git asks helpers in order and stops at the first complete answer (ours).
        let gh = fill("github.com");
        assert!(
            gh.contains("username=octo") && gh.contains("password=tok123"),
            "{gh}"
        );
        let other = fill("git.example.invalid");
        assert!(
            other.contains("password=otherpw") && !other.contains("tok123"),
            "{other}"
        );
        assert!(git_credential().is_none());
    }
}
