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
    api(token, path, None)
}

/// GET (`body` = None) or POST a JSON body to the GitHub REST API.
fn api(token: &str, path: &str, body: Option<serde_json::Value>) -> Result<String, ApiError> {
    api_method(
        token,
        if body.is_some() { "POST" } else { "GET" },
        path,
        body,
    )
}

/// Any REST call (GET / POST / PUT / PATCH / DELETE) with an optional JSON body.
fn api_method(
    token: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<String, ApiError> {
    let url = if path.starts_with("https://") {
        path.to_string()
    } else {
        format!("{API}{path}")
    };
    let auth = format!("Bearer {token}");
    let a = agent();
    macro_rules! send {
        ($req:expr) => {{
            let req = $req
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            match body {
                Some(json) => req.send_json(json),
                None => req.send_empty(),
            }
        }};
    }
    let res = match method {
        "POST" => send!(a.post(&url)),
        "PUT" => send!(a.put(&url)),
        "PATCH" => send!(a.patch(&url)),
        "DELETE" => a
            .delete(&url)
            .header("Authorization", &auth)
            .header("Accept", "application/vnd.github+json")
            .call(),
        _ => a
            .get(&url)
            .header("Authorization", &auth)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call(),
    };
    let mut r = res.map_err(net)?;
    let status = r.status().as_u16();
    let body = r
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| ApiError::Other(e.to_string()))?;
    if (200..300).contains(&status) {
        return Ok(body);
    }
    Err(match status {
        401 => ApiError::Auth(
            "GitHub rejected the token (401) — it may have expired or been revoked".into(),
        ),
        _ => ApiError::Other(format!("GitHub: {} (HTTP {status})", error_message(&body))),
    })
}

fn json<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, ApiError> {
    serde_json::from_str(body)
        .map_err(|e| ApiError::Other(format!("Unexpected answer from GitHub: {e}")))
}

// ---------------------------------------------------------------------------
// Pull requests, issues, checks
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
pub struct Who {
    pub login: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PullRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Label {
    pub name: String,
    #[serde(default)]
    pub color: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Pull {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    pub user: Who,
    pub html_url: String,
    pub head: PullRef,
    pub base: PullRef,
    #[serde(default)]
    pub merged: Option<bool>,
    #[serde(default)]
    pub merged_at: Option<String>,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub mergeable: Option<bool>,
    #[serde(default)]
    pub mergeable_state: Option<String>,
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    #[serde(default)]
    pub changed_files: Option<u64>,
    #[serde(default)]
    pub labels: Vec<Label>,
}

impl Pull {
    pub fn is_merged(&self) -> bool {
        self.merged == Some(true) || self.merged_at.is_some()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct PullFile {
    pub filename: String,
    pub status: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub patch: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Comment {
    pub user: Who,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Review {
    pub user: Who,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    #[serde(default)]
    pub submitted_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    pub user: Who,
    pub html_url: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub comments: u64,
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Pending,
    Success,
    Failure,
    Neutral,
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
    pub url: Option<String>,
}

pub fn list_pulls(token: &str, slug: &str, state: &str) -> Result<Vec<Pull>, ApiError> {
    json(&api_get(
        token,
        &format!("/repos/{slug}/pulls?state={state}&per_page=60&sort=updated&direction=desc"),
    )?)
}

pub fn get_pull(token: &str, slug: &str, n: u64) -> Result<Pull, ApiError> {
    json(&api_get(token, &format!("/repos/{slug}/pulls/{n}"))?)
}

pub fn pull_files(token: &str, slug: &str, n: u64) -> Result<Vec<PullFile>, ApiError> {
    let mut all: Vec<PullFile> = Vec::new();
    for page in 1..=5 {
        let batch: Vec<PullFile> = json(&api_get(
            token,
            &format!("/repos/{slug}/pulls/{n}/files?per_page=100&page={page}"),
        )?)?;
        let len = batch.len();
        all.extend(batch);
        if len < 100 {
            break;
        }
    }
    Ok(all)
}

pub fn issue_comments(token: &str, slug: &str, n: u64) -> Result<Vec<Comment>, ApiError> {
    json(&api_get(
        token,
        &format!("/repos/{slug}/issues/{n}/comments?per_page=100"),
    )?)
}

pub fn pull_reviews(token: &str, slug: &str, n: u64) -> Result<Vec<Review>, ApiError> {
    json(&api_get(
        token,
        &format!("/repos/{slug}/pulls/{n}/reviews?per_page=100"),
    )?)
}

/// GitHub Actions check runs and classic commit statuses for one commit, as one list.
pub fn checks(token: &str, slug: &str, sha: &str) -> Result<Vec<Check>, ApiError> {
    #[derive(Deserialize)]
    struct Run {
        name: String,
        status: String,
        #[serde(default)]
        conclusion: Option<String>,
        #[serde(default)]
        html_url: Option<String>,
    }
    #[derive(Deserialize)]
    struct Runs {
        check_runs: Vec<Run>,
    }
    #[derive(Deserialize)]
    struct Ctx {
        context: String,
        state: String,
        #[serde(default)]
        target_url: Option<String>,
    }
    #[derive(Deserialize)]
    struct Combined {
        statuses: Vec<Ctx>,
    }
    let runs: Runs = json(&api_get(
        token,
        &format!("/repos/{slug}/commits/{sha}/check-runs?per_page=100"),
    )?)?;
    let mut out: Vec<Check> = runs
        .check_runs
        .into_iter()
        .map(|r| Check {
            state: match (r.status.as_str(), r.conclusion.as_deref()) {
                ("completed", Some("success")) => CheckState::Success,
                (
                    "completed",
                    Some(
                        "failure" | "timed_out" | "cancelled" | "action_required"
                        | "startup_failure",
                    ),
                ) => CheckState::Failure,
                ("completed", _) => CheckState::Neutral,
                _ => CheckState::Pending,
            },
            name: r.name,
            url: r.html_url,
        })
        .collect();
    if let Ok(body) = api_get(token, &format!("/repos/{slug}/commits/{sha}/status")) {
        if let Ok(c) = serde_json::from_str::<Combined>(&body) {
            out.extend(c.statuses.into_iter().map(|s| Check {
                state: match s.state.as_str() {
                    "success" => CheckState::Success,
                    "failure" | "error" => CheckState::Failure,
                    _ => CheckState::Pending,
                },
                name: s.context,
                url: s.target_url,
            }));
        }
    }
    Ok(out)
}

pub fn create_pull(
    token: &str,
    slug: &str,
    title: &str,
    body: &str,
    head: &str,
    base: &str,
    draft: bool,
) -> Result<Pull, ApiError> {
    json(&api(
        token,
        &format!("/repos/{slug}/pulls"),
        Some(
            serde_json::json!({"title": title, "body": body, "head": head, "base": base, "draft": draft}),
        ),
    )?)
}

/// method: "merge" | "squash" | "rebase"
pub fn merge_pull(token: &str, slug: &str, n: u64, method: &str) -> Result<(), ApiError> {
    api_method(
        token,
        "PUT",
        &format!("/repos/{slug}/pulls/{n}/merge"),
        Some(serde_json::json!({"merge_method": method})),
    )
    .map(|_| ())
}

pub fn add_comment(token: &str, slug: &str, n: u64, body: &str) -> Result<(), ApiError> {
    api(
        token,
        &format!("/repos/{slug}/issues/{n}/comments"),
        Some(serde_json::json!({"body": body})),
    )
    .map(|_| ())
}

/// event: "APPROVE" | "REQUEST_CHANGES" | "COMMENT"
pub fn submit_review(
    token: &str,
    slug: &str,
    n: u64,
    event: &str,
    body: &str,
) -> Result<(), ApiError> {
    api(
        token,
        &format!("/repos/{slug}/pulls/{n}/reviews"),
        Some(serde_json::json!({"event": event, "body": body})),
    )
    .map(|_| ())
}

/// Close or reopen an issue or pull request ("closed" / "open").
pub fn set_state(token: &str, slug: &str, n: u64, state: &str) -> Result<(), ApiError> {
    api_method(
        token,
        "PATCH",
        &format!("/repos/{slug}/issues/{n}"),
        Some(serde_json::json!({"state": state})),
    )
    .map(|_| ())
}

/// Open issues (GitHub lists pull requests as issues too; those are dropped).
pub fn list_issues(token: &str, slug: &str, state: &str) -> Result<Vec<Issue>, ApiError> {
    let all: Vec<Issue> = json(&api_get(
        token,
        &format!("/repos/{slug}/issues?state={state}&per_page=60&sort=updated&direction=desc"),
    )?)?;
    Ok(all
        .into_iter()
        .filter(|i| i.pull_request.is_none())
        .collect())
}

pub fn get_issue(token: &str, slug: &str, n: u64) -> Result<Issue, ApiError> {
    json(&api_get(token, &format!("/repos/{slug}/issues/{n}"))?)
}

/// The repository's default branch (for a new pull request's base).
pub fn default_branch(token: &str, slug: &str) -> Result<String, ApiError> {
    #[derive(Deserialize)]
    struct R {
        default_branch: String,
    }
    json::<R>(&api_get(token, &format!("/repos/{slug}"))?).map(|r| r.default_branch)
}

/// The human part of a GitHub error response, e.g. "name already exists on this account".
fn error_message(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.chars().take(200).collect();
    };
    let detail: Vec<&str> = v["errors"]
        .as_array()
        .map(|errs| errs.iter().filter_map(|e| e["message"].as_str()).collect())
        .unwrap_or_default();
    let msg = v["message"].as_str().unwrap_or("request failed");
    if detail.is_empty() {
        msg.to_string()
    } else {
        format!("{msg}: {}", detail.join("; "))
    }
}

/// Organizations the account belongs to (where it may be able to create repositories).
pub fn list_orgs(token: &str) -> Result<Vec<String>, ApiError> {
    #[derive(Deserialize)]
    struct Org {
        login: String,
    }
    let body = api_get(token, "/user/orgs?per_page=100")?;
    let orgs: Vec<Org> = serde_json::from_str(&body).map_err(|e| ApiError::Other(e.to_string()))?;
    Ok(orgs.into_iter().map(|o| o.login).collect())
}

/// Create an empty repository for the user (`org` = None) or in an organization.
pub fn create_repo(
    token: &str,
    org: Option<&str>,
    name: &str,
    description: &str,
    private: bool,
) -> Result<GhRepo, ApiError> {
    let path = match org {
        Some(o) => format!("/orgs/{o}/repos"),
        None => "/user/repos".to_string(),
    };
    let body = api(
        token,
        &path,
        Some(serde_json::json!({
            "name": name,
            "description": description,
            "private": private,
            "auto_init": false,
        })),
    )?;
    serde_json::from_str(&body).map_err(|e| ApiError::Other(e.to_string()))
}

/// A repository name GitHub accepts: letters, digits, `-`, `_` and `.` (other characters become `-`).
pub fn sanitize_repo_name(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    s.trim_matches('-').to_string()
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

static GIT_CREDENTIALS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Accounts (login, token) git may use for HTTPS remotes on github.com — the active account
/// first. An empty list stops FoxGit from answering for github.com.
pub fn set_git_credentials(accounts: Vec<(String, String)>) {
    if let Ok(mut c) = GIT_CREDENTIALS.lock() {
        *c = accounts;
    }
}

/// `git -c …` arguments and environment variables for a git child process.
pub type GitCredential = (Vec<String>, Vec<(String, String)>);

/// Picks the account by the user name in the remote URL (`https://alice@github.com/…`, which
/// FoxGit writes when it clones or creates a repository for an account); a URL without a user
/// name gets the active account (index 0). An unknown user name gets no answer, so git asks.
const HELPER: &str = r#"!f() { test "$1" = get || exit 0; u=; while IFS= read -r l; do [ -z "$l" ] && break; case "$l" in username=*) u=${l#username=};; esac; done; i=0; while :; do eval "n=\${FOXGIT_GH_USER_$i-}"; [ -z "$n" ] && break; if [ -z "$u" ] || [ "$n" = "$u" ]; then eval "t=\$FOXGIT_GH_TOKEN_$i"; echo "username=$n"; echo "password=$t"; exit 0; fi; i=$((i+1)); done; }; f"#;

/// Extra `git -c …` arguments and environment for a git child process.
///
/// The empty `helper=` resets the helper list for github.com only (so a stale entry in
/// osxkeychain / Credential Manager can't win), then a tiny inline helper answers with a
/// token from the environment. Other hosts keep the user's own helpers.
pub fn git_credential() -> Option<GitCredential> {
    let c = GIT_CREDENTIALS.lock().ok()?;
    if c.is_empty() {
        return None;
    }
    let mut env = Vec::new();
    for (i, (user, token)) in c.iter().enumerate() {
        env.push((format!("FOXGIT_GH_USER_{i}"), user.clone()));
        env.push((format!("FOXGIT_GH_TOKEN_{i}"), token.clone()));
    }
    Some((
        vec![
            "-c".into(),
            "credential.https://github.com.helper=".into(),
            "-c".into(),
            format!("credential.https://github.com.helper={HELPER}"),
        ],
        env,
    ))
}

/// The user name in an HTTPS URL (`https://alice@github.com/…` → `alice`).
pub fn url_user(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let (auth, _) = rest.split_once('@')?;
    let host_start = rest.find('/').unwrap_or(rest.len());
    (auth.len() < host_start && !auth.is_empty()).then(|| auth.split(':').next().unwrap_or(auth))
}

/// The same HTTPS URL with `user` (or no user) in it; other URLs (SSH) are returned unchanged.
pub fn url_with_user(url: &str, user: Option<&str>) -> String {
    let Some(rest) = url.strip_prefix("https://") else {
        return url.to_string();
    };
    let rest = match url_user(url) {
        Some(_) => rest.split_once('@').map(|(_, r)| r).unwrap_or(rest),
        None => rest,
    };
    match user {
        Some(u) => format!("https://{u}@{rest}"),
        None => format!("https://{rest}"),
    }
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

    /// Parses real GitHub responses into our types (public data, no token; ~8 requests).
    /// `cargo test gh_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn gh_live_responses_parse() {
        let get = |path: &str| -> String {
            let mut res = agent()
                .get(format!("{API}{path}"))
                .header("Accept", "application/vnd.github+json")
                .call()
                .unwrap();
            assert!(res.status().is_success(), "{path}: {}", res.status());
            res.body_mut()
                .with_config()
                .limit(64 << 20)
                .read_to_string()
                .unwrap()
        };
        let slug = "cli/cli";
        let pulls: Vec<Pull> =
            serde_json::from_str(&get(&format!("/repos/{slug}/pulls?state=open&per_page=5")))
                .unwrap();
        assert!(!pulls.is_empty());
        let n = pulls[0].number;
        let pr: Pull = serde_json::from_str(&get(&format!("/repos/{slug}/pulls/{n}"))).unwrap();
        println!(
            "#{} {} +{:?} -{:?} mergeable={:?} {:?}",
            pr.number, pr.title, pr.additions, pr.deletions, pr.mergeable, pr.mergeable_state
        );
        let files: Vec<PullFile> =
            serde_json::from_str(&get(&format!("/repos/{slug}/pulls/{n}/files?per_page=5")))
                .unwrap();
        println!(
            "{} file(s), first: {:?}",
            files.len(),
            files.first().map(|f| &f.filename)
        );
        let _: Vec<Comment> = serde_json::from_str(&get(&format!(
            "/repos/{slug}/issues/{n}/comments?per_page=5"
        )))
        .unwrap();
        let _: Vec<Review> =
            serde_json::from_str(&get(&format!("/repos/{slug}/pulls/{n}/reviews?per_page=5")))
                .unwrap();
        let issues: Vec<Issue> =
            serde_json::from_str(&get(&format!("/repos/{slug}/issues?state=open&per_page=5")))
                .unwrap();
        println!(
            "{} issue(s)/PRs, {} real issues",
            issues.len(),
            issues.iter().filter(|i| i.pull_request.is_none()).count()
        );
        let runs: serde_json::Value = serde_json::from_str(&get(&format!(
            "/repos/{slug}/commits/{}/check-runs?per_page=5",
            pr.head.sha
        )))
        .unwrap();
        println!("check runs: {}", runs["total_count"]);
    }

    use super::*;

    #[test]
    fn repo_names_and_errors() {
        assert_eq!(sanitize_repo_name(" my app (v2) "), "my-app--v2");
        assert_eq!(sanitize_repo_name("foxgit"), "foxgit");
        assert_eq!(
            error_message(
                r#"{"message":"Repository creation failed.","errors":[{"message":"name already exists on this account"}]}"#
            ),
            "Repository creation failed.: name already exists on this account"
        );
        assert_eq!(error_message(r#"{"message":"Not Found"}"#), "Not Found");
    }

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
    fn url_users() {
        assert_eq!(url_user("https://alice@github.com/o/r.git"), Some("alice"));
        assert_eq!(url_user("https://github.com/o/r.git"), None);
        assert_eq!(url_user("git@github.com:o/r.git"), None);
        assert_eq!(
            url_with_user("https://github.com/o/r.git", Some("bob")),
            "https://bob@github.com/o/r.git"
        );
        assert_eq!(
            url_with_user("https://alice@github.com/o/r.git", Some("bob")),
            "https://bob@github.com/o/r.git"
        );
        assert_eq!(
            url_with_user("https://alice@github.com/o/r.git", None),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            url_with_user("git@github.com:o/r.git", Some("bob")),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn git_picks_the_account_by_url_user() {
        set_git_credentials(vec![
            ("octo".into(), "tok-active".into()),
            ("night-cat".into(), "tok-second".into()),
        ]);
        let (args, env) = git_credential().unwrap();
        set_git_credentials(Vec::new());
        let dir = tempfile::tempdir().unwrap();
        let fill = |desc: &str| {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(&args)
                .args([
                    "-c",
                    "credential.helper=!f() { echo username=other; echo password=otherpw; }; f",
                ])
                .args(["credential", "fill"])
                .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
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
                        .write_all(format!("{desc}\n\n").as_bytes())?;
                    c.wait_with_output()
                })
                .unwrap();
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        // no user in the URL → the active account
        let a = fill("protocol=https\nhost=github.com");
        assert!(
            a.contains("username=octo") && a.contains("password=tok-active"),
            "{a}"
        );
        // user in the URL → that account's token
        let b = fill("protocol=https\nhost=github.com\nusername=night-cat");
        assert!(b.contains("password=tok-second"), "{b}");
        // a user we don't know → we stay quiet and the next helper answers
        let c = fill("protocol=https\nhost=github.com\nusername=stranger");
        assert!(c.contains("password=otherpw") && !c.contains("tok-"), "{c}");
        // other hosts are never answered by us
        let d = fill("protocol=https\nhost=git.example.invalid");
        assert!(d.contains("password=otherpw") && !d.contains("tok-"), "{d}");
        assert!(git_credential().is_none());
    }
}
