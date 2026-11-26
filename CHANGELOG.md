# Changelog

All notable changes to FoxGit. The release workflow copies the section of the tagged version
into the GitHub Release notes.

## [0.1.0] - 2026-11-26

First public release.

### Highlights
- Fast Sourcetree-style Git client for macOS (Apple Silicon + Intel), Windows and Linux.
- Project tabs with branch, change count and ahead/behind at a glance; tabs come back on restart.
- Commit graph with branch / remote / tag badges, search, and resizable columns.
- Branches grouped into folders by their `/` prefix (`feature/…`, `fix/…`), local and remote.
- Stage, unstage and discard whole files, single hunks or individual lines.
- GitHub account: sign in with the browser (device code) or a personal access token; the token
  is kept in the system keychain and used automatically for HTTPS remotes on github.com.
- Projects → Remote: every repository your GitHub account can access, with one-click Clone.
- New dark theme (default) and a matching light theme.

### Also
- Auto-fetch of all open projects, credential / passphrase prompts, stash, tags, cherry-pick,
  revert, reset, merge, rebase.
- Uses the login shell's `PATH` when started from Finder / a launcher, so git hooks (husky),
  gpg, git-lfs and credential helpers work like in a terminal.
- Warns when Git is not installed, checks for new versions once a day, and keeps a crash log.
