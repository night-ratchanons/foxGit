//! Apps started from Finder / the Dock / a desktop launcher get a minimal `PATH`
//! (`/usr/bin:/bin:/usr/sbin:/sbin` on macOS), so git hooks that run `npx` (husky, lint-staged),
//! `gpg` for signing, `git-lfs` and credential helpers installed by Homebrew are not found and
//! commits or pushes fail although they work in a terminal.
//!
//! At startup we ask the user's login shell for its `PATH` once (in the background) and give
//! that to every git child process. The git availability check runs after it.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

static LOGIN_PATH: OnceLock<Option<String>> = OnceLock::new();

/// Start resolving the login-shell `PATH` (no-op on Windows, where installers set PATH globally).
pub fn init() {
    std::thread::spawn(|| {
        let _ = LOGIN_PATH.set(resolve());
    });
}

/// The merged `PATH` for child processes, once known.
pub fn path() -> Option<&'static str> {
    LOGIN_PATH.get().and_then(|p| p.as_deref())
}

/// Block until the login-shell `PATH` is known (for background threads only).
pub fn wait_path() -> Option<&'static str> {
    LOGIN_PATH.wait().as_deref()
}

#[cfg(windows)]
fn resolve() -> Option<String> {
    None
}

#[cfg(not(windows))]
fn resolve() -> Option<String> {
    let current = std::env::var("PATH").unwrap_or_default();
    let login = login_shell_path().unwrap_or_default();
    let merged = merge_paths(&login, &current);
    (merged != current).then_some(merged)
}

#[cfg(not(windows))]
fn login_shell_path() -> Option<String> {
    use std::io::Read;
    use std::time::{Duration, Instant};
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "/bin/zsh".into()
            } else {
                "/bin/sh".into()
            }
        });
    const MARK: &str = "__FOXGIT_PATH__";
    let mut child = Command::new(&shell)
        // interactive + login: read the same startup files a terminal window does
        .args(["-ilc", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    // A slow or stuck shell config must not keep git from working.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let text = reader.join().ok()?;
    let start = text.find(MARK)? + MARK.len();
    let end = start + text[start..].find(MARK)?;
    Some(text[start..end].to_string())
}

/// Login-shell entries first (that's what a terminal would use), then anything else we had.
#[cfg_attr(windows, allow(dead_code))]
pub fn merge_paths(first: &str, then: &str) -> String {
    let sep = if cfg!(windows) { ';' } else { ':' };
    let mut seen = std::collections::HashSet::new();
    first
        .split(sep)
        .chain(then.split(sep))
        .filter(|p| !p.is_empty() && seen.insert(p.to_string()))
        .collect::<Vec<_>>()
        .join(&sep.to_string())
}

/// `git --version`, or a message explaining how to install Git.
pub fn check_git() -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(p) = wait_path() {
        cmd.env("PATH", p);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    match cmd.output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::merge_paths;

    #[test]
    #[cfg(not(windows))]
    fn merge_keeps_order_and_drops_duplicates() {
        assert_eq!(
            merge_paths("/opt/homebrew/bin:/usr/bin", "/usr/bin:/bin::/usr/bin"),
            "/opt/homebrew/bin:/usr/bin:/bin"
        );
    }
}
