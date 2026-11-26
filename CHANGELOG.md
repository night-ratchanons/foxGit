# Changelog

All notable changes to FoxGit. The release workflow copies the section of the tagged version
into the GitHub Release notes.

## [Unreleased]

### New: AI agents in FoxGit
- **AGENTS** in the sidebar → **New agent chat…**: give a task to **Claude Code**, **Codex** or
  **Hermes** (whichever CLIs are installed) and chat with it. Each chat works in its own git
  worktree on an `agent/…` branch, so your folder is never touched.
- Live progress (what it says, files it edits, commands it runs), Stop, and replies at any time —
  messages typed while it's busy wait in a queue; Claude Code and Hermes resume the same session.
- **Review changes** opens the agent's copy as a tab (stage / discard hunks and lines as usual),
  then **Merge into <branch>** (or squash into staged changes) or **Discard**.

## [0.1.0] - 2026-11-26

First public release.

### Highlights
- Fast Sourcetree-style Git client for macOS (Apple Silicon + Intel), Windows and Linux.
- Project tabs with branch, change count and ahead/behind at a glance; tabs come back on restart.
- Commit graph with branch / remote / tag badges, search, and resizable columns.
- Branches grouped into folders by their `/` prefix (`feature/…`, `fix/…`), local and remote.
- Stage, unstage and discard whole files, single hunks or individual lines.
- Remotes: add one to a repository created locally (or create a new GitHub repository for it
  in one step), edit its name / URL, remove it; Commit & Push sets the upstream on the first push.
- GitHub accounts: connect one or more with a personal access token (kept in the system
  keychain) and switch between them from the toolbar. HTTPS remotes on github.com use the
  account named in the URL (FoxGit writes it when cloning / creating), otherwise the active one.
- Projects → Remote: every repository each GitHub account can access, grouped by account,
  with one-click Clone.
- New dark theme (default) and a matching light theme.

### Also
- Auto-fetch of all open projects, credential / passphrase prompts, stash, tags, cherry-pick,
  revert, reset, merge, rebase.
- Uses the login shell's `PATH` when started from Finder / a launcher, so git hooks (husky),
  gpg, git-lfs and credential helpers work like in a terminal.
- Warns when Git is not installed, checks for new versions once a day, and keeps a crash log.
