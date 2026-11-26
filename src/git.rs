//! Git backend.
//!
//! * Reading (history, refs, status, diffs) uses libgit2 through `git2` — fast and in-process.
//! * Writing (stage, commit, checkout, merge, fetch/pull/push, stash…) runs the user's `git`
//!   command, so hooks, signing, SSH keys and credential helpers all work exactly as in a terminal.

use git2::{
    BranchType, Delta, DiffOptions, ErrorCode, Oid, Repository, Sort, Status, StatusOptions,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub type Result<T> = std::result::Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CommitInfo {
    pub oid: Oid,
    pub short: String,
    pub summary: String,
    pub message: String,
    pub author: String,
    pub email: String,
    pub committer: String,
    /// Seconds since epoch.
    pub time: i64,
    /// Offset from UTC in minutes.
    pub offset_min: i32,
    pub parents: Vec<Oid>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefKind {
    Head,
    Local,
    Remote,
    Tag,
}

#[derive(Clone, Debug)]
pub struct RefLabel {
    pub name: String,
    pub kind: RefKind,
}

#[derive(Clone, Debug)]
pub struct LocalBranch {
    pub name: String,
    pub is_head: bool,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub target: Option<Oid>,
}

#[derive(Clone, Debug)]
pub struct RemoteBranch {
    /// Full short name, e.g. `origin/main`.
    pub name: String,
    pub remote: String,
    pub branch: String,
    pub target: Option<Oid>,
}

#[derive(Clone, Debug)]
pub struct StashInfo {
    pub index: usize,
    pub message: String,
    pub oid: Oid,
}

#[derive(Clone, Debug, Default)]
pub struct Refs {
    pub head_name: Option<String>,
    pub head_detached: bool,
    pub head_oid: Option<Oid>,
    pub locals: Vec<LocalBranch>,
    pub remotes: Vec<RemoteBranch>,
    pub remote_names: Vec<String>,
    /// Fetch URL of each remote (by name).
    pub remote_urls: HashMap<String, String>,
    pub tags: Vec<(String, Oid)>,
    pub stashes: Vec<StashInfo>,
    pub labels: HashMap<Oid, Vec<RefLabel>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChange,
    Untracked,
    Conflicted,
}

impl ChangeKind {
    pub fn letter(self) -> &'static str {
        match self {
            ChangeKind::Added => "A",
            ChangeKind::Modified => "M",
            ChangeKind::Deleted => "D",
            ChangeKind::Renamed => "R",
            ChangeKind::TypeChange => "T",
            ChangeKind::Untracked => "?",
            ChangeKind::Conflicted => "!",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug, Default)]
pub struct WorkStatus {
    pub staged: Vec<FileChange>,
    pub unstaged: Vec<FileChange>,
}

impl WorkStatus {
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    Hunk,
    Meta,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// Display text (lossy UTF-8, no line ending).
    pub text: String,
    /// Exact bytes of the line without the trailing `\n` (keeps `\r` and non-UTF-8 encodings),
    /// used to build patches for hunk / line staging.
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Hunk {
    /// Index of the `@@` header line in `FileDiff::lines`.
    pub header: usize,
    /// One past the last line of this hunk.
    pub end: usize,
    pub old_start: u32,
    pub new_start: u32,
}

#[derive(Clone, Debug, Default)]
pub struct FileDiff {
    pub lines: Vec<DiffLine>,
    pub hunks: Vec<Hunk>,
    pub old_path: Vec<u8>,
    pub new_path: Vec<u8>,
    pub binary: bool,
    pub truncated: bool,
    pub added: usize,
    pub removed: usize,
}

impl FileDiff {
    /// Line number (new side) of the first change, for "open in editor".
    pub fn first_new_line(&self) -> Option<usize> {
        let first = self
            .lines
            .iter()
            .position(|l| l.kind == LineKind::Added || l.kind == LineKind::Removed)?;
        self.lines[first..]
            .iter()
            .chain(self.lines[..first].iter().rev())
            .find_map(|l| l.new_no)
            .map(|n| n as usize)
    }

    pub fn hunk_of(&self, line: usize) -> Option<usize> {
        self.hunks
            .iter()
            .position(|h| line > h.header && line < h.end)
    }

    /// Build a patch containing only the selected changed lines of the given hunks.
    ///
    /// * forward (stage): unselected `-` lines become context, unselected `+` lines are dropped.
    /// * reverse (unstage / discard): unselected `+` lines become context, unselected `-` are dropped.
    ///
    /// Returns `None` when nothing would change. Apply with `git apply --recount`.
    pub fn build_patch(
        &self,
        hunks: &[usize],
        selected: &dyn Fn(usize) -> bool,
        reverse: bool,
    ) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(b"diff --git a/");
        out.extend_from_slice(&self.old_path);
        out.extend_from_slice(b" b/");
        out.extend_from_slice(&self.new_path);
        out.extend_from_slice(b"\n--- a/");
        out.extend_from_slice(&self.old_path);
        out.extend_from_slice(b"\n+++ b/");
        out.extend_from_slice(&self.new_path);
        out.push(b'\n');

        let mut any = false;
        for &hi in hunks {
            let Some(h) = self.hunks.get(hi) else {
                continue;
            };
            let mut body: Vec<u8> = Vec::new();
            let (mut old_n, mut new_n, mut changes) = (0u32, 0u32, 0usize);
            let mut last_emitted = false;
            for (i, l) in (h.header + 1..h.end).zip(&self.lines[h.header + 1..h.end]) {
                let emit: Option<u8> = match l.kind {
                    LineKind::Context => Some(b' '),
                    LineKind::Added if selected(i) => Some(b'+'),
                    LineKind::Removed if selected(i) => Some(b'-'),
                    LineKind::Added => reverse.then_some(b' '),
                    LineKind::Removed => (!reverse).then_some(b' '),
                    LineKind::Meta => {
                        // "\ No newline at end of file" belongs to the line before it
                        if last_emitted {
                            body.extend_from_slice(b"\\ No newline at end of file\n");
                        }
                        continue;
                    }
                    LineKind::Hunk => None,
                };
                last_emitted = emit.is_some();
                if let Some(c) = emit {
                    match c {
                        b' ' => {
                            old_n += 1;
                            new_n += 1;
                        }
                        b'+' => {
                            new_n += 1;
                            changes += 1;
                        }
                        _ => {
                            old_n += 1;
                            changes += 1;
                        }
                    }
                    body.push(c);
                    body.extend_from_slice(&l.raw);
                    body.push(b'\n');
                }
            }
            if changes == 0 {
                continue;
            }
            any = true;
            out.extend_from_slice(
                format!(
                    "@@ -{},{} +{},{} @@\n",
                    h.old_start, old_n, h.new_start, new_n
                )
                .as_bytes(),
            );
            out.extend_from_slice(&body);
        }
        any.then_some(out)
    }
}

/// Which diff the user is looking at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffTarget {
    Commit { oid: Oid, path: String },
    Staged { path: String },
    Unstaged { path: String },
}

const MAX_DIFF_LINES: usize = 30_000;

// ---------------------------------------------------------------------------
// Repository wrapper
// ---------------------------------------------------------------------------

pub struct Repo {
    pub repo: Repository,
    pub workdir: PathBuf,
}

impl Repo {
    pub fn open(path: &Path) -> Result<Self> {
        let repo = Repository::discover(path).map_err(e)?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| "Bare repositories are not supported".to_string())?
            .to_path_buf();
        Ok(Self { repo, workdir })
    }

    pub fn name(&self) -> String {
        self.workdir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.workdir.display().to_string())
    }

    /// Load up to `limit` commits reachable from all branches, remotes and tags,
    /// in topological + date order (newest first).
    pub fn commits(&self, limit: usize) -> Result<Vec<CommitInfo>> {
        let mut walk = self.repo.revwalk().map_err(e)?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
            .map_err(e)?;
        // HEAD may be unborn (new repo) or detached; branches/tags are pushed below anyway.
        let _ = walk.push_head();
        let _ = walk.push_glob("refs/heads/*");
        let _ = walk.push_glob("refs/remotes/*");
        let _ = walk.push_glob("refs/tags/*");

        let mut out = Vec::with_capacity(limit.min(4096));
        for oid in walk {
            if out.len() >= limit {
                break;
            }
            let oid = match oid {
                Ok(o) => o,
                Err(_) => continue,
            };
            let c = match self.repo.find_commit(oid) {
                Ok(c) => c,
                Err(_) => continue, // e.g. a tag pointing at a tree/blob
            };
            let author = c.author();
            let committer = c.committer();
            let message = c.message().unwrap_or("").to_string();
            let summary = message.lines().next().unwrap_or("").to_string();
            out.push(CommitInfo {
                oid,
                short: oid.to_string()[..7].to_string(),
                summary,
                message,
                author: author.name().unwrap_or("").to_string(),
                email: author.email().unwrap_or("").to_string(),
                committer: committer.name().unwrap_or("").to_string(),
                time: author.when().seconds(),
                offset_min: author.when().offset_minutes(),
                parents: c.parent_ids().collect(),
            });
        }
        Ok(out)
    }

    pub fn refs(&mut self) -> Result<Refs> {
        let mut refs = Refs::default();

        match self.repo.head() {
            Ok(h) => {
                refs.head_oid = h.target();
                refs.head_detached = self.repo.head_detached().unwrap_or(false);
                refs.head_name = h.shorthand().ok().map(|s| s.to_string());
            }
            Err(_) => {
                // Unborn branch: read the symbolic name from HEAD.
                if let Ok(r) = self.repo.find_reference("HEAD") {
                    if let Ok(Some(t)) = r.symbolic_target() {
                        refs.head_name = Some(t.trim_start_matches("refs/heads/").to_string());
                    }
                }
            }
        }

        if let Some(oid) = refs.head_oid {
            if refs.head_detached {
                refs.labels.entry(oid).or_default().push(RefLabel {
                    name: "HEAD".into(),
                    kind: RefKind::Head,
                });
            }
        }

        for b in self.repo.branches(Some(BranchType::Local)).map_err(e)? {
            let (b, _) = b.map_err(e)?;
            let name = b.name().ok().flatten().unwrap_or("").to_string();
            let target = b.get().target();
            let is_head = b.is_head();
            let (upstream, ahead, behind) = match b.upstream() {
                Ok(up) => {
                    let up_name = up.name().ok().flatten().map(|s| s.to_string());
                    let (a, bh) = match (target, up.get().target()) {
                        (Some(l), Some(u)) => self.repo.graph_ahead_behind(l, u).unwrap_or((0, 0)),
                        _ => (0, 0),
                    };
                    (up_name, a, bh)
                }
                Err(_) => (None, 0, 0),
            };
            if let Some(t) = target {
                refs.labels.entry(t).or_default().push(RefLabel {
                    name: name.clone(),
                    kind: if is_head {
                        RefKind::Head
                    } else {
                        RefKind::Local
                    },
                });
            }
            refs.locals.push(LocalBranch {
                name,
                is_head,
                upstream,
                ahead,
                behind,
                target,
            });
        }

        for b in self.repo.branches(Some(BranchType::Remote)).map_err(e)? {
            let (b, _) = b.map_err(e)?;
            let name = b.name().ok().flatten().unwrap_or("").to_string();
            if name.ends_with("/HEAD") {
                continue;
            }
            let target = b.get().target();
            let (remote, branch) = match name.split_once('/') {
                Some((r, br)) => (r.to_string(), br.to_string()),
                None => (String::new(), name.clone()),
            };
            if let Some(t) = target {
                refs.labels.entry(t).or_default().push(RefLabel {
                    name: name.clone(),
                    kind: RefKind::Remote,
                });
            }
            refs.remotes.push(RemoteBranch {
                name,
                remote,
                branch,
                target,
            });
        }

        if let Ok(names) = self.repo.remotes() {
            refs.remote_names = names
                .iter()
                .flatten()
                .flatten()
                .map(|s| s.to_string())
                .collect();
        }
        for name in &refs.remote_names {
            if let Some(url) = self
                .repo
                .find_remote(name)
                .ok()
                .and_then(|r| r.url().ok().map(String::from))
            {
                refs.remote_urls.insert(name.clone(), url);
            }
        }

        let mut tags = Vec::new();
        let _ = self.repo.tag_foreach(|oid, name| {
            let name = String::from_utf8_lossy(name)
                .trim_start_matches("refs/tags/")
                .to_string();
            tags.push((name, oid));
            true
        });
        for (name, oid) in tags {
            // Peel annotated tags to the commit they point at.
            let target = self
                .repo
                .find_object(oid, None)
                .and_then(|o| o.peel_to_commit())
                .map(|c| c.id())
                .unwrap_or(oid);
            refs.labels.entry(target).or_default().push(RefLabel {
                name: name.clone(),
                kind: RefKind::Tag,
            });
            refs.tags.push((name, target));
        }
        refs.tags.sort_by(|a, b| b.0.cmp(&a.0));

        let mut stashes = Vec::new();
        let _ = self.repo.stash_foreach(|i, msg, oid| {
            stashes.push(StashInfo {
                index: i,
                message: msg.to_string(),
                oid: *oid,
            });
            true
        });
        refs.stashes = stashes;

        // Keep HEAD branch first, then local, remote, tags.
        for v in refs.labels.values_mut() {
            v.sort_by_key(|l| match l.kind {
                RefKind::Head => 0,
                RefKind::Local => 1,
                RefKind::Remote => 2,
                RefKind::Tag => 3,
            });
        }
        Ok(refs)
    }

    pub fn status(&self) -> Result<WorkStatus> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false)
            .renames_head_to_index(false);
        let statuses = self.repo.statuses(Some(&mut opts)).map_err(e)?;
        let mut ws = WorkStatus::default();
        for s in statuses.iter() {
            let path = match s.path() {
                Ok(p) => p.to_string(),
                Err(_) => String::from_utf8_lossy(s.path_bytes()).to_string(),
            };
            let st = s.status();
            if st.contains(Status::CONFLICTED) {
                ws.unstaged.push(FileChange {
                    path,
                    kind: ChangeKind::Conflicted,
                });
                continue;
            }
            let staged = if st.contains(Status::INDEX_NEW) {
                Some(ChangeKind::Added)
            } else if st.contains(Status::INDEX_MODIFIED) {
                Some(ChangeKind::Modified)
            } else if st.contains(Status::INDEX_DELETED) {
                Some(ChangeKind::Deleted)
            } else if st.contains(Status::INDEX_RENAMED) {
                Some(ChangeKind::Renamed)
            } else if st.contains(Status::INDEX_TYPECHANGE) {
                Some(ChangeKind::TypeChange)
            } else {
                None
            };
            let unstaged = if st.contains(Status::WT_NEW) {
                Some(ChangeKind::Untracked)
            } else if st.contains(Status::WT_MODIFIED) {
                Some(ChangeKind::Modified)
            } else if st.contains(Status::WT_DELETED) {
                Some(ChangeKind::Deleted)
            } else if st.contains(Status::WT_RENAMED) {
                Some(ChangeKind::Renamed)
            } else if st.contains(Status::WT_TYPECHANGE) {
                Some(ChangeKind::TypeChange)
            } else {
                None
            };
            if let Some(k) = staged {
                ws.staged.push(FileChange {
                    path: path.clone(),
                    kind: k,
                });
            }
            if let Some(k) = unstaged {
                ws.unstaged.push(FileChange { path, kind: k });
            }
        }
        ws.staged.sort_by(|a, b| a.path.cmp(&b.path));
        ws.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(ws)
    }

    /// Files changed by a commit (compared with its first parent).
    pub fn commit_files(&self, oid: Oid) -> Result<Vec<FileChange>> {
        let commit = self.repo.find_commit(oid).map_err(e)?;
        let tree = commit.tree().map_err(e)?;
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
        let mut diff = self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
            .map_err(e)?;
        let _ = diff.find_similar(None);
        let mut files = Vec::new();
        for d in diff.deltas() {
            let kind = match d.status() {
                Delta::Added => ChangeKind::Added,
                Delta::Deleted => ChangeKind::Deleted,
                Delta::Renamed => ChangeKind::Renamed,
                Delta::Typechange => ChangeKind::TypeChange,
                _ => ChangeKind::Modified,
            };
            let file = if kind == ChangeKind::Deleted {
                d.old_file()
            } else {
                d.new_file()
            };
            let path = file
                .path()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            files.push(FileChange { path, kind });
        }
        Ok(files)
    }

    pub fn diff(&self, target: &DiffTarget, context_lines: u32) -> Result<FileDiff> {
        let mut opts = DiffOptions::new();
        opts.context_lines(context_lines);
        let diff = match target {
            DiffTarget::Commit { oid, path } => {
                opts.pathspec(path).disable_pathspec_match(true);
                let commit = self.repo.find_commit(*oid).map_err(e)?;
                let tree = commit.tree().map_err(e)?;
                let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
                self.repo
                    .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
                    .map_err(e)?
            }
            DiffTarget::Staged { path } => {
                opts.pathspec(path).disable_pathspec_match(true);
                let head_tree = self.repo.head().ok().and_then(|h| h.peel_to_tree().ok());
                self.repo
                    .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))
                    .map_err(e)?
            }
            DiffTarget::Unstaged { path } => {
                opts.pathspec(path)
                    .disable_pathspec_match(true)
                    .include_untracked(true)
                    .recurse_untracked_dirs(true)
                    .show_untracked_content(true);
                self.repo
                    .diff_index_to_workdir(None, Some(&mut opts))
                    .map_err(e)?
            }
        };

        let mut fd = FileDiff::default();
        let mut stopped = false;
        let res = diff.print(git2::DiffFormat::Patch, |delta, hunk, line| {
            if delta.flags().is_binary() {
                fd.binary = true;
            }
            if fd.old_path.is_empty() {
                let path_bytes =
                    |f: git2::DiffFile| f.path_bytes().map(|b| b.to_vec()).unwrap_or_default();
                fd.old_path = path_bytes(delta.old_file());
                fd.new_path = path_bytes(delta.new_file());
            }
            if fd.lines.len() >= MAX_DIFF_LINES {
                stopped = true;
                return false;
            }
            let content = line.content();
            let raw = content.strip_suffix(b"\n").unwrap_or(content).to_vec();
            let mut text = String::from_utf8_lossy(&raw)
                .trim_end_matches('\r')
                .to_string();
            let kind = match line.origin() {
                '+' => {
                    fd.added += 1;
                    LineKind::Added
                }
                '-' => {
                    fd.removed += 1;
                    LineKind::Removed
                }
                ' ' => LineKind::Context,
                'H' => {
                    if let Some(h) = hunk {
                        if let Some(last) = fd.hunks.last_mut() {
                            last.end = fd.lines.len();
                        }
                        fd.hunks.push(Hunk {
                            header: fd.lines.len(),
                            end: fd.lines.len() + 1,
                            old_start: h.old_start(),
                            new_start: h.new_start(),
                        });
                    }
                    LineKind::Hunk
                }
                'F' => {
                    // File header ("diff --git ...", "index ...") — not shown.
                    return true;
                }
                'B' => {
                    fd.binary = true;
                    return true;
                }
                '=' | '>' | '<' => {
                    text = "\\ No newline at end of file".into();
                    LineKind::Meta
                }
                _ => LineKind::Meta,
            };
            fd.lines.push(DiffLine {
                kind,
                old_no: line.old_lineno(),
                new_no: line.new_lineno(),
                text,
                raw,
            });
            true
        });
        if let Some(last) = fd.hunks.last_mut() {
            last.end = fd.lines.len();
        }
        if let Err(err) = res {
            if !stopped {
                return Err(e(err));
            }
        }
        fd.truncated = stopped;
        Ok(fd)
    }

    pub fn is_head_unborn(&self) -> bool {
        matches!(self.repo.head(), Err(err) if err.code() == ErrorCode::UnbornBranch || err.code() == ErrorCode::NotFound)
    }

    /// Last commit message (for "amend").
    pub fn head_message(&self) -> Option<String> {
        self.repo
            .head()
            .ok()?
            .peel_to_commit()
            .ok()?
            .message()
            .ok()
            .map(|s| s.trim_end().to_string())
    }
}

// ---------------------------------------------------------------------------
// Git CLI (write operations)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub ok: bool,
    pub text: String,
}

/// Run `git <args>` inside `dir`, optionally feeding `stdin`.
pub fn run_git(dir: &Path, args: &[String], stdin: Option<&[u8]>) -> CmdOutput {
    run_git_ext(dir, args, stdin, false)
}

/// `background = true` for automatic jobs (auto-fetch): credential prompts are answered from
/// the session cache only and never pop up a dialog.
pub fn run_git_ext(
    dir: &Path,
    args: &[String],
    stdin: Option<&[u8]>,
    background: bool,
) -> CmdOutput {
    let mut cmd = Command::new("git");
    // Signed in to GitHub? HTTPS remotes on github.com use that account's token.
    // (`-c` options must come before the subcommand.)
    if let Some((pre, env)) = crate::github::git_credential() {
        cmd.args(pre).envs(env);
    }
    // Started from Finder / a launcher: use the login shell's PATH so hooks, gpg, git-lfs and
    // credential helpers resolve like in a terminal (also picks Homebrew's git if present).
    if let Some(path) = crate::shellenv::path() {
        cmd.env("PATH", path);
    }
    cmd.current_dir(dir)
        .args(args)
        // Never block waiting for a password prompt in a terminal we don't have.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .envs(crate::askpass::child_env(background))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            return CmdOutput {
                ok: false,
                text: format!("Could not run git: {err}\nIs Git installed and on your PATH?"),
            }
        }
    };
    if let Some(input) = stdin {
        use std::io::Write;
        if let Some(mut s) = child.stdin.take() {
            let _ = s.write_all(input);
        }
    }
    match child.wait_with_output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            CmdOutput {
                ok: out.status.success(),
                text: text.trim_end().to_string(),
            }
        }
        Err(err) => CmdOutput {
            ok: false,
            text: err.to_string(),
        },
    }
}

/// A write operation, executed on a background thread.
#[derive(Debug, Clone)]
pub enum Op {
    Stage(Vec<String>),
    StageAll,
    Unstage {
        paths: Vec<String>,
        unborn: bool,
    },
    UnstageAll {
        unborn: bool,
    },
    Discard {
        tracked: Vec<String>,
        untracked: Vec<String>,
    },
    Commit {
        message: String,
        amend: bool,
        push: bool,
        /// Push with `-u <remote> <branch>` (the branch has no upstream yet).
        upstream: Option<(String, String)>,
    },
    Checkout(String),
    CheckoutRemote {
        remote_branch: String,
        local: String,
    },
    CheckoutCommit(String),
    CreateBranch {
        name: String,
        start: Option<String>,
        checkout: bool,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    Merge(String),
    Rebase(String),
    Fetch,
    Pull {
        rebase: bool,
    },
    Push {
        set_upstream: Option<(String, String)>,
        force: bool,
    },
    /// Push `local` to `remote_branch` on `remote` (picked in the Push dialog).
    PushTo {
        remote: String,
        local: String,
        remote_branch: String,
        set_upstream: bool,
        force: bool,
    },
    StashSave(String),
    StashPop(usize),
    StashApply(usize),
    StashDrop(usize),
    CreateTag {
        name: String,
        commit: String,
    },
    DeleteTag(String),
    ResetTo {
        commit: String,
        mode: String,
    },
    Revert(String),
    CherryPick(String),
    RemoteAdd {
        name: String,
        url: String,
        fetch: bool,
    },
    /// Rename and/or change the URL of a remote.
    RemoteEdit {
        name: String,
        new_name: Option<String>,
        url: Option<String>,
    },
    RemoteRemove(String),
    /// Bring an agent's work in: commit what's uncommitted in its worktree, then merge (or
    /// squash) its branch into the current branch of the main repository.
    AgentMerge {
        worktree: String,
        branch: String,
        message: String,
        commit_first: bool,
        squash: bool,
    },
    /// Commit what's uncommitted in an agent's worktree, then push its branch with upstream.
    AgentPush {
        worktree: String,
        branch: String,
        remote: String,
        message: String,
        commit_first: bool,
    },
    /// Bring the base branch's new commits into an agent's worktree (`git merge <base>`).
    AgentSync {
        worktree: String,
        base: String,
    },
    /// Delete an agent's worktree (and its branch).
    AgentCleanup {
        worktree: String,
        branch: String,
        delete_branch: bool,
    },
    /// Fetch pull request `number` from GitHub into the local branch `pr/<number>` and switch to it.
    CheckoutPr {
        remote: String,
        number: u64,
        /// The local `pr/<number>` branch already exists (then it is fast-forwarded).
        exists: bool,
    },
    /// Resolve conflicted files with one side as a whole (`ours` or `theirs`) and mark them resolved.
    ResolveWith {
        paths: Vec<String>,
        theirs: bool,
    },
    /// Mark conflicted files as resolved (`git add`).
    MarkResolved(Vec<String>),
    /// Resolve one file in the external merge tool git is set up with.
    MergeTool(String),
    /// Continue the merge / rebase / cherry-pick / revert in progress.
    Continue(Sequence),
    /// Abort the merge / rebase / cherry-pick / revert in progress.
    Abort(Sequence),
    /// Repository-local `user.name` / `user.email` (empty = leave unchanged).
    SetIdentity {
        name: String,
        email: String,
    },
    /// Stage / unstage / discard part of a file (hunks or individual lines).
    ApplyPatch {
        patch: Vec<u8>,
        cached: bool,
        reverse: bool,
        zero_context: bool,
        label: String,
    },
}

/// An operation that stops on conflicts and can be continued or aborted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sequence {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl Sequence {
    /// The one in progress in `repo`, if any.
    pub fn of(state: git2::RepositoryState) -> Option<Sequence> {
        use git2::RepositoryState as S;
        match state {
            S::Merge => Some(Sequence::Merge),
            S::Rebase | S::RebaseInteractive | S::RebaseMerge => Some(Sequence::Rebase),
            S::CherryPick | S::CherryPickSequence => Some(Sequence::CherryPick),
            S::Revert | S::RevertSequence => Some(Sequence::Revert),
            _ => None,
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Sequence::Merge => "merge",
            Sequence::Rebase => "rebase",
            Sequence::CherryPick => "cherry-pick",
            Sequence::Revert => "revert",
        }
    }

    pub fn noun(self) -> &'static str {
        match self {
            Sequence::Merge => "merge",
            Sequence::Rebase => "rebase",
            Sequence::CherryPick => "cherry-pick",
            Sequence::Revert => "revert",
        }
    }
}

impl Op {
    pub fn title(&self) -> String {
        match self {
            Op::Stage(p) => format!("Stage {} file(s)", p.len()),
            Op::StageAll => "Stage all".into(),
            Op::Unstage { paths, .. } => format!("Unstage {} file(s)", paths.len()),
            Op::UnstageAll { .. } => "Unstage all".into(),
            Op::Discard { tracked, untracked } => {
                format!("Discard {} file(s)", tracked.len() + untracked.len())
            }
            Op::Commit { amend, .. } => if *amend { "Amend commit" } else { "Commit" }.into(),
            Op::Checkout(b) => format!("Checkout {b}"),
            Op::CheckoutRemote { local, .. } => format!("Checkout {local}"),
            Op::CheckoutCommit(c) => format!("Checkout {}", &c[..7.min(c.len())]),
            Op::CreateBranch { name, .. } => format!("Create branch {name}"),
            Op::DeleteBranch { name, .. } => format!("Delete branch {name}"),
            Op::Merge(b) => format!("Merge {b}"),
            Op::Rebase(b) => format!("Rebase onto {b}"),
            Op::Fetch => "Fetch".into(),
            Op::Pull { .. } => "Pull".into(),
            Op::Push { .. } => "Push".into(),
            Op::PushTo {
                remote,
                local,
                remote_branch,
                ..
            } => {
                if local == remote_branch {
                    format!("Push {local} to {remote}")
                } else {
                    format!("Push {local} to {remote}/{remote_branch}")
                }
            }
            Op::StashSave(_) => "Stash".into(),
            Op::StashPop(i) => format!("Pop stash@{{{i}}}"),
            Op::StashApply(i) => format!("Apply stash@{{{i}}}"),
            Op::StashDrop(i) => format!("Drop stash@{{{i}}}"),
            Op::CreateTag { name, .. } => format!("Create tag {name}"),
            Op::DeleteTag(t) => format!("Delete tag {t}"),
            Op::ResetTo { mode, .. } => format!("Reset ({mode})"),
            Op::Revert(c) => format!("Revert {}", &c[..7.min(c.len())]),
            Op::CherryPick(c) => format!("Cherry-pick {}", &c[..7.min(c.len())]),
            Op::RemoteAdd { name, .. } => format!("Add remote {name}"),
            Op::RemoteEdit { name, .. } => format!("Edit remote {name}"),
            Op::RemoteRemove(name) => format!("Remove remote {name}"),
            Op::AgentMerge { branch, squash, .. } => {
                format!("{} {branch}", if *squash { "Squash" } else { "Merge" })
            }
            Op::AgentCleanup { branch, .. } => format!("Discard {branch}"),
            Op::AgentPush { branch, .. } => format!("Push {branch}"),
            Op::AgentSync { base, .. } => format!("Update agent's copy from {base}"),
            Op::CheckoutPr { number, .. } => format!("Checkout pull request #{number}"),
            Op::ResolveWith { paths, theirs } => format!(
                "Resolve {} file(s) using {}",
                paths.len(),
                if *theirs { "theirs" } else { "ours" }
            ),
            Op::MarkResolved(p) => format!("Mark {} file(s) resolved", p.len()),
            Op::MergeTool(p) => format!("Merge tool: {p}"),
            Op::Continue(q) => format!("Continue {}", q.noun()),
            Op::Abort(q) => format!("Abort {}", q.noun()),
            Op::SetIdentity { .. } => "Set author identity".into(),
            Op::ApplyPatch { label, .. } => label.clone(),
        }
    }

    /// Execute: returns the commands run with their output.
    pub fn run(&self, dir: &Path) -> CmdOutput {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let with_paths = |base: &[&str], paths: &[String]| {
            let mut v = s(base);
            v.push("--".into());
            v.extend(paths.iter().cloned());
            v
        };

        let steps: Vec<(Vec<String>, Option<Vec<u8>>)> = match self {
            Op::Stage(paths) => vec![(with_paths(&["add", "-A"], paths), None)],
            Op::StageAll => vec![(s(&["add", "-A"]), None)],
            Op::Unstage { paths, unborn } => {
                if *unborn {
                    vec![(with_paths(&["rm", "--cached", "-r", "-q"], paths), None)]
                } else {
                    vec![(with_paths(&["reset", "-q", "HEAD"], paths), None)]
                }
            }
            Op::UnstageAll { unborn } => {
                if *unborn {
                    vec![(s(&["rm", "--cached", "-r", "-q", "."]), None)]
                } else {
                    vec![(s(&["reset", "-q"]), None)]
                }
            }
            Op::Discard { tracked, untracked } => {
                let mut v = Vec::new();
                if !tracked.is_empty() {
                    v.push((with_paths(&["checkout", "-q"], tracked), None));
                }
                if !untracked.is_empty() {
                    v.push((with_paths(&["clean", "-f", "-q"], untracked), None));
                }
                v
            }
            Op::Commit {
                message,
                amend,
                push,
                upstream,
            } => {
                let mut c = s(&["commit", "-F", "-"]);
                if *amend {
                    c.push("--amend".into());
                }
                let mut v = vec![(c, Some(message.clone().into_bytes()))];
                if *push {
                    v.push((
                        match upstream {
                            Some((remote, branch)) => s(&["push", "-u", remote, branch]),
                            None => s(&["push"]),
                        },
                        None,
                    ));
                }
                v
            }
            Op::Checkout(b) => vec![(s(&["checkout", b]), None)],
            Op::CheckoutRemote {
                remote_branch,
                local,
            } => vec![(
                s(&["checkout", "-b", local, "--track", remote_branch]),
                None,
            )],
            Op::CheckoutCommit(c) => vec![(s(&["checkout", "--detach", c]), None)],
            Op::CreateBranch {
                name,
                start,
                checkout,
            } => {
                let mut c = if *checkout {
                    s(&["checkout", "-b", name])
                } else {
                    s(&["branch", name])
                };
                if let Some(st) = start {
                    c.push(st.clone());
                }
                vec![(c, None)]
            }
            Op::DeleteBranch { name, force } => {
                vec![(s(&["branch", if *force { "-D" } else { "-d" }, name]), None)]
            }
            Op::Merge(b) => vec![(s(&["merge", "--no-edit", b]), None)],
            Op::Rebase(b) => vec![(s(&["rebase", b]), None)],
            Op::Fetch => vec![(s(&["fetch", "--all", "--prune", "--tags"]), None)],
            Op::Pull { rebase } => vec![(
                s(&["pull", if *rebase { "--rebase" } else { "--no-rebase" }]),
                None,
            )],
            Op::Push {
                set_upstream,
                force,
            } => {
                let mut c = s(&["push"]);
                if *force {
                    c.push("--force-with-lease".into());
                }
                if let Some((remote, branch)) = set_upstream {
                    c.push("-u".into());
                    c.push(remote.clone());
                    c.push(branch.clone());
                }
                vec![(c, None)]
            }
            Op::PushTo {
                remote,
                local,
                remote_branch,
                set_upstream,
                force,
            } => {
                let mut c = s(&["push"]);
                if *force {
                    c.push("--force-with-lease".into());
                }
                if *set_upstream {
                    c.push("-u".into());
                }
                c.push(remote.clone());
                // full ref names: also creates a remote branch with a different name
                c.push(format!("refs/heads/{local}:refs/heads/{remote_branch}"));
                vec![(c, None)]
            }
            Op::StashSave(msg) => {
                let mut c = s(&["stash", "push", "--include-untracked"]);
                if !msg.trim().is_empty() {
                    c.push("-m".into());
                    c.push(msg.clone());
                }
                vec![(c, None)]
            }
            Op::StashPop(i) => vec![(s(&["stash", "pop", &format!("stash@{{{i}}}")]), None)],
            Op::StashApply(i) => vec![(s(&["stash", "apply", &format!("stash@{{{i}}}")]), None)],
            Op::StashDrop(i) => vec![(s(&["stash", "drop", &format!("stash@{{{i}}}")]), None)],
            Op::CreateTag { name, commit } => vec![(s(&["tag", name, commit]), None)],
            Op::DeleteTag(t) => vec![(s(&["tag", "-d", t]), None)],
            Op::ResetTo { commit, mode } => {
                vec![(s(&["reset", &format!("--{mode}"), commit]), None)]
            }
            Op::Revert(c) => vec![(s(&["revert", "--no-edit", c]), None)],
            Op::CherryPick(c) => vec![(s(&["cherry-pick", c]), None)],
            Op::RemoteAdd { name, url, fetch } => {
                let mut v = vec![(s(&["remote", "add", name, url]), None)];
                if *fetch {
                    v.push((s(&["fetch", name]), None));
                }
                v
            }
            Op::RemoteEdit {
                name,
                new_name,
                url,
            } => {
                let mut v = Vec::new();
                let mut current = name.clone();
                if let Some(n) = new_name.as_ref().filter(|n| *n != name) {
                    v.push((s(&["remote", "rename", name, n]), None));
                    current = n.clone();
                }
                if let Some(u) = url {
                    v.push((s(&["remote", "set-url", &current, u]), None));
                }
                v
            }
            Op::RemoteRemove(name) => vec![(s(&["remote", "remove", name]), None)],
            Op::AgentMerge {
                worktree,
                branch,
                message,
                commit_first,
                squash,
            } => {
                let mut v = Vec::new();
                if *commit_first {
                    v.push((s(&["-C", worktree, "add", "-A"]), None));
                    v.push((
                        s(&["-C", worktree, "commit", "-F", "-"]),
                        Some(message.clone().into_bytes()),
                    ));
                }
                if *squash {
                    v.push((s(&["merge", "--squash", branch]), None));
                } else {
                    v.push((
                        s(&["merge", "--no-ff", "-m", &format!("Merge {branch}"), branch]),
                        None,
                    ));
                }
                v
            }
            Op::AgentPush {
                worktree,
                branch,
                remote,
                message,
                commit_first,
            } => {
                let mut v = Vec::new();
                if *commit_first {
                    v.push((s(&["-C", worktree, "add", "-A"]), None));
                    v.push((
                        s(&["-C", worktree, "commit", "-F", "-"]),
                        Some(message.clone().into_bytes()),
                    ));
                }
                v.push((s(&["push", "-u", remote, branch]), None));
                v
            }
            Op::AgentSync { worktree, base } => {
                vec![(s(&["-C", worktree, "merge", "--no-edit", base]), None)]
            }
            Op::AgentCleanup {
                worktree,
                branch,
                delete_branch,
            } => {
                let mut v = vec![(s(&["worktree", "remove", "--force", worktree]), None)];
                if *delete_branch {
                    v.push((s(&["branch", "-D", branch]), None));
                }
                v
            }
            Op::CheckoutPr {
                remote,
                number,
                exists,
            } => {
                let local = format!("pr/{number}");
                let tracking = format!("{remote}/pr/{number}");
                let mut v = vec![(
                    s(&[
                        "fetch",
                        remote,
                        &format!("+pull/{number}/head:refs/remotes/{tracking}"),
                    ]),
                    None,
                )];
                if *exists {
                    v.push((s(&["checkout", &local]), None));
                    v.push((s(&["merge", "--ff-only", &tracking]), None));
                } else {
                    v.push((
                        s(&["checkout", "--no-track", "-b", &local, &tracking]),
                        None,
                    ));
                }
                v
            }
            Op::ResolveWith { paths, theirs } => vec![
                (
                    with_paths(
                        &["checkout", if *theirs { "--theirs" } else { "--ours" }],
                        paths,
                    ),
                    None,
                ),
                (with_paths(&["add"], paths), None),
            ],
            Op::MarkResolved(paths) => vec![(with_paths(&["add"], paths), None)],
            Op::MergeTool(path) => vec![(
                with_paths(
                    &[
                        "-c",
                        "mergetool.keepBackup=false",
                        "mergetool",
                        "--no-prompt",
                    ],
                    std::slice::from_ref(path),
                ),
                None,
            )],
            Op::Continue(q) => vec![(
                s(&["-c", "core.editor=true", q.command(), "--continue"]),
                None,
            )],
            Op::Abort(q) => vec![(s(&[q.command(), "--abort"]), None)],
            Op::SetIdentity { name, email } => {
                let mut v = Vec::new();
                if !name.trim().is_empty() {
                    v.push((s(&["config", "user.name", name.trim()]), None));
                }
                if !email.trim().is_empty() {
                    v.push((s(&["config", "user.email", email.trim()]), None));
                }
                v
            }
            Op::ApplyPatch {
                patch,
                cached,
                reverse,
                zero_context,
                ..
            } => {
                let mut c = s(&["apply", "--recount", "--whitespace=nowarn"]);
                if *cached {
                    c.push("--cached".into());
                }
                if *reverse {
                    c.push("--reverse".into());
                }
                if *zero_context {
                    c.push("--unidiff-zero".into());
                }
                c.push("-".into());
                vec![(c, Some(patch.clone()))]
            }
        };

        let mut log = String::new();
        for (args, input) in steps {
            log.push_str(&format!("$ git {}\n", args.join(" ")));
            let out = run_git(dir, &args, input.as_deref());
            if !out.text.is_empty() {
                log.push_str(&out.text);
                log.push('\n');
            }
            if !out.ok {
                return CmdOutput {
                    ok: false,
                    text: log.trim_end().to_string(),
                };
            }
        }
        CmdOutput {
            ok: true,
            text: log.trim_end().to_string(),
        }
    }
}

pub fn init_repo(path: &Path) -> Result<()> {
    Repository::init(path).map(|_| ()).map_err(e)
}

pub fn clone_repo(url: &str, dest_parent: &Path, dir_name: &str) -> CmdOutput {
    let args = vec![
        "clone".to_string(),
        "--progress".to_string(),
        url.to_string(),
        dir_name.to_string(),
    ];
    let mut out = run_git(dest_parent, &args, None);
    out.text = format!("$ git clone {url} {dir_name}\n{}", out.text);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(dir: &Path, args: &[&str]) {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let out = run_git(dir, &a, None);
        assert!(out.ok, "git {args:?} failed: {}", out.text);
    }

    fn setup() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        git(d, &["init", "-q", "-b", "main"]);
        git(d, &["config", "user.name", "Tester"]);
        git(d, &["config", "user.email", "t@example.com"]);
        git(d, &["config", "commit.gpgsign", "false"]);
        fs::write(d.join("a.txt"), "one\ntwo\n").unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "first"]);
        git(d, &["checkout", "-q", "-b", "feature"]);
        fs::write(d.join("b.txt"), "สวัสดี\n").unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "add b"]);
        git(d, &["checkout", "-q", "main"]);
        fs::write(d.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(d, &["commit", "-q", "-am", "edit a"]);
        git(d, &["merge", "-q", "--no-edit", "feature"]);
        git(d, &["tag", "v1"]);
        t
    }

    #[test]
    fn history_refs_and_graph() {
        let t = setup();
        let mut repo = Repo::open(t.path()).unwrap();
        let commits = repo.commits(100).unwrap();
        assert_eq!(commits.len(), 4);
        assert_eq!(commits[0].parents.len(), 2, "newest commit is the merge");
        let refs = repo.refs().unwrap();
        assert_eq!(refs.head_name.as_deref(), Some("main"));
        assert_eq!(refs.locals.len(), 2);
        assert_eq!(refs.tags.len(), 1);
        assert!(refs.labels[&commits[0].oid]
            .iter()
            .any(|l| l.kind == RefKind::Head));
        let input: Vec<_> = commits.iter().map(|c| (c.oid, c.parents.clone())).collect();
        let rows = crate::graph::layout(&input);
        assert_eq!(rows.iter().map(|r| r.width).max(), Some(2));
    }

    #[test]
    fn stage_commit_and_diff() {
        let t = setup();
        let d = t.path();
        fs::write(d.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        fs::write(d.join("new.txt"), "hello\n").unwrap();
        let repo = Repo::open(d).unwrap();
        let st = repo.status().unwrap();
        assert_eq!(st.unstaged.len(), 2);
        assert!(st.staged.is_empty());

        let diff = repo
            .diff(
                &DiffTarget::Unstaged {
                    path: "a.txt".into(),
                },
                3,
            )
            .unwrap();
        assert_eq!((diff.added, diff.removed), (1, 1));
        let untracked = repo
            .diff(
                &DiffTarget::Unstaged {
                    path: "new.txt".into(),
                },
                3,
            )
            .unwrap();
        assert_eq!(untracked.added, 1);

        assert!(Op::Stage(vec!["a.txt".into()]).run(d).ok);
        let st = repo.status().unwrap();
        assert_eq!(st.staged.len(), 1);
        let staged = repo
            .diff(
                &DiffTarget::Staged {
                    path: "a.txt".into(),
                },
                3,
            )
            .unwrap();
        assert_eq!(staged.added, 1);

        let out = Op::Commit {
            message: "change two\n".into(),
            amend: false,
            push: false,
            upstream: None,
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        let commits = repo.commits(10).unwrap();
        assert_eq!(commits[0].summary, "change two");
        let files = repo.commit_files(commits[0].oid).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");

        assert!(
            Op::Discard {
                tracked: vec![],
                untracked: vec!["new.txt".into()]
            }
            .run(d)
            .ok
        );
        assert!(repo.status().unwrap().is_clean());
    }

    fn apply(
        d: &Path,
        diff: &FileDiff,
        hunks: &[usize],
        sel: &dyn Fn(usize) -> bool,
        cached: bool,
        reverse: bool,
    ) {
        let patch = diff.build_patch(hunks, sel, reverse).expect("patch");
        let out = Op::ApplyPatch {
            patch: patch.clone(),
            cached,
            reverse,
            zero_context: false,
            label: "t".into(),
        }
        .run(d);
        assert!(
            out.ok,
            "{}\n---\n{}",
            out.text,
            String::from_utf8_lossy(&patch)
        );
    }

    fn git_out(d: &Path, args: &[&str]) -> String {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        run_git(d, &a, None).text
    }

    #[test]
    fn hunk_and_line_staging() {
        let t = setup();
        let d = t.path();
        let base: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        fs::write(d.join("big file ไทย.txt"), &base).unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "base"]);
        // two separate hunks: edit near the top and near the bottom
        let edited = base
            .replace("line 2\n", "line 2 changed\n")
            .replace("line 28\n", "line 28\nnew A\nnew B\n");
        fs::write(d.join("big file ไทย.txt"), &edited).unwrap();

        let repo = Repo::open(d).unwrap();
        let path = "big file ไทย.txt".to_string();
        let diff = repo
            .diff(&DiffTarget::Unstaged { path: path.clone() }, 3)
            .unwrap();
        assert_eq!(diff.hunks.len(), 2);

        // Stage only the 2nd hunk
        apply(d, &diff, &[1], &|_| true, true, false);
        let cached = git_out(d, &["diff", "--cached"]);
        assert!(
            cached.contains("+new A") && !cached.contains("line 2 changed"),
            "{cached}"
        );

        // Unstage just the line "new B" from the staged hunk
        let staged = repo
            .diff(&DiffTarget::Staged { path: path.clone() }, 3)
            .unwrap();
        let b = staged.lines.iter().position(|l| l.text == "new B").unwrap();
        apply(
            d,
            &staged,
            &[staged.hunk_of(b).unwrap()],
            &|i| i == b,
            true,
            true,
        );
        let cached = git_out(d, &["diff", "--cached"]);
        assert!(
            cached.contains("+new A") && !cached.contains("+new B"),
            "{cached}"
        );

        // Stage only the removed half of hunk 1 ("-line 2" without "+line 2 changed")
        let unstaged = repo
            .diff(&DiffTarget::Unstaged { path: path.clone() }, 3)
            .unwrap();
        let minus = unstaged
            .lines
            .iter()
            .position(|l| l.kind == LineKind::Removed)
            .unwrap();
        apply(
            d,
            &unstaged,
            &[unstaged.hunk_of(minus).unwrap()],
            &|i| i == minus,
            true,
            false,
        );
        let cached = git_out(d, &["diff", "--cached"]);
        assert!(
            cached.contains("-line 2") && !cached.contains("+line 2 changed"),
            "{cached}"
        );

        // Discard the "new B" line from the working tree
        let unstaged = repo
            .diff(&DiffTarget::Unstaged { path: path.clone() }, 3)
            .unwrap();
        let b = unstaged
            .lines
            .iter()
            .position(|l| l.text == "new B")
            .unwrap();
        apply(
            d,
            &unstaged,
            &[unstaged.hunk_of(b).unwrap()],
            &|i| i == b,
            false,
            true,
        );
        let content = fs::read_to_string(d.join(&path)).unwrap();
        assert!(content.contains("new A") && !content.contains("new B"));
        assert!(
            content.contains("line 2 changed"),
            "untouched change survives"
        );
    }

    #[test]
    fn crlf_and_missing_newline_survive_hunk_staging() {
        let t = setup();
        let d = t.path();
        git(d, &["config", "core.autocrlf", "false"]);
        fs::write(d.join("w.txt"), b"a\r\nb\r\nc").unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "crlf"]);
        fs::write(d.join("w.txt"), b"a\r\nB\r\nc2").unwrap();
        let repo = Repo::open(d).unwrap();
        let diff = repo
            .diff(
                &DiffTarget::Unstaged {
                    path: "w.txt".into(),
                },
                3,
            )
            .unwrap();
        apply(d, &diff, &[0], &|_| true, true, false);
        assert!(
            repo.status().unwrap().unstaged.is_empty(),
            "everything staged exactly"
        );
        let staged_blob = git_out(d, &["show", ":w.txt"]);
        assert_eq!(staged_blob, "a\r\nB\r\nc2");
    }

    #[test]
    fn remotes_add_edit_remove_and_first_push() {
        let t = setup();
        let d = t.path();
        let bare = tempfile::tempdir().unwrap();
        git(bare.path(), &["init", "-q", "--bare"]);
        let url = bare.path().display().to_string();
        let add = Op::RemoteAdd {
            name: "origin".into(),
            url: url.clone(),
            fetch: true,
        }
        .run(d);
        assert!(add.ok, "{}", add.text);
        let mut repo = Repo::open(d).unwrap();
        let refs = repo.refs().unwrap();
        assert_eq!(refs.remote_names, ["origin"]);
        assert_eq!(refs.remote_urls["origin"], url);

        // Commit & Push on a branch without upstream sets it up
        fs::write(d.join("c.txt"), "c\n").unwrap();
        git(d, &["add", "c.txt"]);
        let out = Op::Commit {
            message: "add c\n".into(),
            amend: false,
            push: true,
            upstream: Some(("origin".into(), "main".into())),
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        let up = git_out(d, &["rev-parse", "--abbrev-ref", "main@{upstream}"]);
        assert_eq!(up.trim(), "origin/main");

        let other = tempfile::tempdir().unwrap();
        git(other.path(), &["init", "-q", "--bare"]);
        let url2 = other.path().display().to_string();
        let edit = Op::RemoteEdit {
            name: "origin".into(),
            new_name: Some("github".into()),
            url: Some(url2.clone()),
        }
        .run(d);
        assert!(edit.ok, "{}", edit.text);
        let refs = repo.refs().unwrap();
        assert_eq!(refs.remote_names, ["github"]);
        assert_eq!(refs.remote_urls["github"], url2);

        let rm = Op::RemoteRemove("github".into()).run(d);
        assert!(rm.ok, "{}", rm.text);
        assert!(repo.refs().unwrap().remote_names.is_empty());
    }

    #[test]
    fn agent_worktree_merge_and_cleanup() {
        let t = setup();
        let d = t.path();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt = wt_dir.path().join("task");
        let wts = wt.display().to_string();
        git(d, &["worktree", "add", "-b", "agent/task", &wts, "main"]);
        // the agent edits without committing
        fs::write(wt.join("agent.txt"), "from the agent\n").unwrap();
        let out = Op::AgentMerge {
            worktree: wts.clone(),
            branch: "agent/task".into(),
            message: "Agent work\n".into(),
            commit_first: true,
            squash: false,
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        assert_eq!(
            fs::read_to_string(d.join("agent.txt")).unwrap(),
            "from the agent\n"
        );
        assert!(git_out(d, &["log", "-1", "--format=%s"]).contains("Merge agent/task"));

        let out = Op::AgentCleanup {
            worktree: wts.clone(),
            branch: "agent/task".into(),
            delete_branch: true,
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        assert!(!wt.exists());
        assert!(git_out(d, &["branch", "--list", "agent/task"])
            .trim()
            .is_empty());
    }

    #[test]
    fn agent_push_commits_then_pushes_the_branch() {
        let t = setup();
        let d = t.path();
        let bare = tempfile::tempdir().unwrap();
        git(bare.path(), &["init", "-q", "--bare"]);
        git(
            d,
            &[
                "remote",
                "add",
                "origin",
                &bare.path().display().to_string(),
            ],
        );
        let wt_dir = tempfile::tempdir().unwrap();
        let wt = wt_dir.path().join("task");
        let wts = wt.display().to_string();
        git(d, &["worktree", "add", "-b", "agent/push", &wts, "main"]);
        fs::write(wt.join("x.txt"), "x\n").unwrap();
        let out = Op::AgentPush {
            worktree: wts,
            branch: "agent/push".into(),
            remote: "origin".into(),
            message: "Agent work\n".into(),
            commit_first: true,
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        let remote = git_out(bare.path(), &["log", "-1", "--format=%s", "agent/push"]);
        assert_eq!(remote.trim(), "Agent work");
        let up = git_out(d, &["rev-parse", "--abbrev-ref", "agent/push@{upstream}"]);
        assert_eq!(up.trim(), "origin/agent/push");
    }

    #[test]
    fn agent_sync_merges_new_base_commits() {
        let t = setup();
        let d = t.path();
        let wt_dir = tempfile::tempdir().unwrap();
        let wt = wt_dir.path().join("task");
        let wts = wt.display().to_string();
        git(
            d,
            &["worktree", "add", "-q", "-b", "agent/sync", &wts, "main"],
        );
        fs::write(wt.join("agent.txt"), "agent\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-q", "-m", "agent work"]);
        fs::write(d.join("base.txt"), "base\n").unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "base moved on"]);
        let out = Op::AgentSync {
            worktree: wts,
            base: "main".into(),
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        assert!(wt.join("base.txt").exists() && wt.join("agent.txt").exists());
    }

    #[test]
    fn push_to_a_picked_remote_branch() {
        let t = setup();
        let d = t.path();
        let bare = tempfile::tempdir().unwrap();
        git(bare.path(), &["init", "-q", "--bare"]);
        git(
            d,
            &[
                "remote",
                "add",
                "origin",
                &bare.path().display().to_string(),
            ],
        );
        // a branch that isn't checked out, to a remote branch with another name, no upstream
        let out = Op::PushTo {
            remote: "origin".into(),
            local: "feature".into(),
            remote_branch: "review/feature".into(),
            set_upstream: false,
            force: false,
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        assert_eq!(
            git_out(bare.path(), &["log", "-1", "--format=%s", "review/feature"]).trim(),
            "add b"
        );
        assert!(git_out(d, &["config", "--get", "branch.feature.remote"])
            .trim()
            .is_empty());
        // same name with upstream
        let out = Op::PushTo {
            remote: "origin".into(),
            local: "main".into(),
            remote_branch: "main".into(),
            set_upstream: true,
            force: false,
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        assert_eq!(
            git_out(d, &["rev-parse", "--abbrev-ref", "main@{upstream}"]).trim(),
            "origin/main"
        );
    }

    #[test]
    fn checkout_pr_fetches_pull_head_into_local_branch() {
        // the "GitHub" side: a repo whose refs/pull/7/head points at a contributor's commit
        let t = setup();
        let d = t.path();
        let server = setup();
        let sd = server.path();
        git(sd, &["checkout", "-q", "-b", "contrib"]);
        fs::write(sd.join("pr.txt"), "from a fork\n").unwrap();
        git(sd, &["add", "."]);
        git(sd, &["commit", "-q", "-m", "pr work"]);
        git(sd, &["update-ref", "refs/pull/7/head", "contrib"]);
        git(sd, &["checkout", "-q", "main"]);
        git(d, &["remote", "add", "origin", &sd.display().to_string()]);

        let out = Op::CheckoutPr {
            remote: "origin".into(),
            number: 7,
            exists: false,
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        assert_eq!(git_out(d, &["branch", "--show-current"]).trim(), "pr/7");
        assert_eq!(git_out(d, &["log", "-1", "--format=%s"]).trim(), "pr work");
        // no upstream: a plain Push must not create pr/7 on the remote by accident
        assert!(git_out(d, &["config", "--get", "branch.pr/7.remote"])
            .trim()
            .is_empty());

        // the author pushes more; checking out again fast-forwards the existing branch
        git(sd, &["checkout", "-q", "contrib"]);
        fs::write(sd.join("pr.txt"), "from a fork\nfixup\n").unwrap();
        git(sd, &["commit", "-q", "-am", "pr fixup"]);
        git(sd, &["update-ref", "refs/pull/7/head", "contrib"]);
        git(d, &["checkout", "-q", "main"]);
        let out = Op::CheckoutPr {
            remote: "origin".into(),
            number: 7,
            exists: true,
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        assert_eq!(git_out(d, &["branch", "--show-current"]).trim(), "pr/7");
        assert_eq!(git_out(d, &["log", "-1", "--format=%s"]).trim(), "pr fixup");
    }

    #[test]
    fn resolve_conflicts_and_continue_merge() {
        let t = setup();
        let d = t.path();
        git(d, &["checkout", "-q", "-b", "other"]);
        fs::write(d.join("a.txt"), "theirs\n").unwrap();
        git(d, &["commit", "-q", "-am", "theirs"]);
        git(d, &["checkout", "-q", "main"]);
        fs::write(d.join("a.txt"), "ours\n").unwrap();
        git(d, &["commit", "-q", "-am", "ours"]);
        let _ = Op::Merge("other".into()).run(d); // stops with a conflict
        let repo = Repo::open(d).unwrap();
        assert_eq!(Sequence::of(repo.repo.state()), Some(Sequence::Merge));
        let status = repo.status().unwrap();
        assert!(status
            .unstaged
            .iter()
            .any(|f| f.kind == ChangeKind::Conflicted));

        let out = Op::ResolveWith {
            paths: vec!["a.txt".into()],
            theirs: true,
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "theirs\n");
        let out = Op::Continue(Sequence::Merge).run(d);
        assert!(out.ok, "{}", out.text);
        let repo = Repo::open(d).unwrap();
        assert_eq!(Sequence::of(repo.repo.state()), None);
        assert_eq!(
            git_out(d, &["log", "-1", "--format=%p"])
                .split_whitespace()
                .count(),
            2
        );
    }

    #[test]
    fn abort_rebase_and_set_identity() {
        let t = setup();
        let d = t.path();
        git(d, &["checkout", "-q", "-b", "other", "HEAD~2"]);
        fs::write(d.join("a.txt"), "clash\n").unwrap();
        git(d, &["commit", "-q", "-am", "clash"]);
        let _ = Op::Rebase("main".into()).run(d);
        let repo = Repo::open(d).unwrap();
        assert_eq!(Sequence::of(repo.repo.state()), Some(Sequence::Rebase));
        let out = Op::Abort(Sequence::Rebase).run(d);
        assert!(out.ok, "{}", out.text);
        assert_eq!(git_out(d, &["branch", "--show-current"]).trim(), "other");

        let out = Op::SetIdentity {
            name: " New Name ".into(),
            email: String::new(),
        }
        .run(d);
        assert!(out.ok, "{}", out.text);
        assert_eq!(git_out(d, &["config", "user.name"]).trim(), "New Name");
        assert_eq!(
            git_out(d, &["config", "user.email"]).trim(),
            "t@example.com"
        );
    }

    #[test]
    fn unborn_repo() {
        let t = tempfile::tempdir().unwrap();
        init_repo(t.path()).unwrap();
        let mut repo = Repo::open(t.path()).unwrap();
        assert!(repo.is_head_unborn());
        assert!(repo.commits(10).unwrap().is_empty());
        let refs = repo.refs().unwrap();
        assert!(refs.head_name.is_some());
        fs::write(t.path().join("x"), "x").unwrap();
        assert!(Op::StageAll.run(t.path()).ok);
        assert_eq!(repo.status().unwrap().staged.len(), 1);
        assert!(Op::UnstageAll { unborn: true }.run(t.path()).ok);
        assert_eq!(repo.status().unwrap().unstaged.len(), 1);
    }
}

#[cfg(test)]
mod bench {
    /// `RT_BENCH=/path/to/repo cargo test --release bench_load -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_load() {
        let path = std::env::var("RT_BENCH").expect("set RT_BENCH");
        let t = std::time::Instant::now();
        let mut repo = super::Repo::open(std::path::Path::new(&path)).unwrap();
        let commits = repo.commits(100_000).unwrap();
        let t_commits = t.elapsed();
        let input: Vec<_> = commits.iter().map(|c| (c.oid, c.parents.clone())).collect();
        let rows = crate::graph::layout(&input);
        let t_graph = t.elapsed();
        let refs = repo.refs().unwrap();
        println!(
            "{} commits in {:?}, graph ({} rows, max {} lanes) at {:?}, {} branches, total {:?}",
            commits.len(),
            t_commits,
            rows.len(),
            rows.iter().map(|r| r.width).max().unwrap_or(0),
            t_graph,
            refs.locals.len() + refs.remotes.len(),
            t.elapsed()
        );
    }
}
