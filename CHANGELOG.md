# Changelog

All notable changes to FoxGit. The release workflow copies the section of the tagged version
into the GitHub Release notes.

## [Unreleased]

### New: AI agents in FoxGit
- Clear zones: an activity rail on the far left switches between **Changes**, **History** and
  **Agents**; each mode has its own sidebar (branches/remotes/tags/stashes, or the agent chats).
- **Agents** → **New chat**: give a task to **Claude Code**, **Codex** or
  **Hermes** (whichever CLIs are installed) and chat with it. Each chat works in its own git
  worktree on an `agent/…` branch, so your folder is never touched.
- Live progress (what it says, files it edits, commands it runs), Stop, and replies at any time —
  messages typed while it's busy wait in a queue; Claude Code and Hermes resume the same session.
- **Review changes** opens the agent's copy as a tab (stage / discard hunks and lines as usual),
  then **Merge into <branch>** (or squash into staged changes) or **Discard**.
- Developer tools per chat (Orca-style): a built-in **Terminal** tab (a real PTY shell in the
  agent's copy, with "Continue in Claude Code / Hermes" to open the agent's own full-screen
  interface on the same session), **Open in Cursor / VS Code / Zed**, **Open in Terminal app**,
  and **Push & PR** (commit, push the branch, open GitHub's new pull request page).

### New: a bigger Agents workspace
- Every agent chat is a workspace with three tabs: **Chat**, **Changes** (everything the agent
  changed since its base, file by file — select lines, write comments, send them all to the
  agent as one review) and **Terminal** (several terminals in the agent's copy: its own
  full-screen interface, shells, and the project's scripts with Open in browser).
- New chat: **Chat** or **Terminal** mode, pick a **model** and a **profile**, **Plan first**,
  and run the same task on **several agents / several times** at once — they start side by side
  and the **board** compares them (status, changes, cost, last message, Merge / Discard).
- Message box: prompt **chips** (Plan first, Write tests, Review yourself, Explain … plus your
  own from Settings), **@ attach files**, model switch and **Plan mode** per chat.
- Chat menu: Rename, **Retry with another agent**, **Update from <base>** (merge the base's new
  commits into the agent's copy), **Run tests**.
- **Profiles** (Settings → Agent profiles): separate accounts / configs per agent CLI
  (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `HERMES_HOME`), with Sign in….
- Quitting FoxGit stops running agents (the next message resumes the same session).
- Push dialog: pick the local branch, the remote and the remote branch (existing or new) from
  lists; the picked target is used even without "set upstream".

### New: everything in one window
- **GitHub mode** (new rail button): the repository's **pull requests** and **issues**, open or
  closed, with a filter. A pull request shows its conversation (description, comments, reviews),
  **Files** (the patch of every file) and **Checks** (CI status). Merge (merge commit, squash or
  rebase), **Approve**, **Request changes**, comment, close / reopen, **Checkout** (fetches the
  PR into a local `pr/<number>` branch, also for PRs from forks) and Open on GitHub.
- **New pull request** right from FoxGit: title, description, base branch (GitHub's default
  branch is picked), draft; the branch is pushed first when needed. An agent's **Push & PR** now
  fills in the same form instead of opening the browser. An issue has **Ask an agent**: it
  starts an agent chat with the issue as the task.
- **CI status** of the commit selected in History (✔ / × / running, with links to each check).
- **Conflict resolution**: a conflicted file opens a view with one card per conflict — **Use
  ours**, **Use theirs**, **Both** (either order) or neither — then **Save & mark resolved**;
  or take a whole side, open the file in your editor, or run git's merge tool. The banner says
  what is in progress (merge, rebase, cherry-pick, revert) with **Continue** and **Abort**.
- **Run panel** (toolbar ▶ Run): the project's `package.json` scripts (with pnpm / yarn / bun /
  npm from the lockfile), Cargo, Makefile targets and Go run in terminals docked at the bottom;
  when a dev server prints its `localhost` URL an **Open in browser** button appears.
- **Search mode** (⇧⌘F / Ctrl+Shift+F): find in files with `git grep` (match case, whole word,
  regex, untracked files); a click opens your editor at that line.
- **✨ Write message**: Claude Code writes the commit message from the staged diff, in the
  style of the repository's recent commits.
- **Command palette** (⌘K on macOS, Ctrl+Shift+P): every mode, git action, branch to check out,
  script, project and setting, with fuzzy search.
- **Settings** (⌘, / Ctrl+,): theme, terminal font size, code editor, default agent,
  auto-fetch, and the author name / email of the current repository.
- **Dashboard** (Projects → Dashboard): every open project — branch, ahead/behind, changes,
  conflicts, running scripts and agent chats — with one click to the right place.
- **Quick edit**: fix a working-copy file inside FoxGit (diff header → Edit, ⌘S saves), or ↗ to
  open it in your editor at the first change.
- **One window**: `foxgit <folder>` while FoxGit is running opens the folder as a tab in the
  running window. Folders can also be dropped onto the window.
- System notifications (and a Dock bounce) when an agent finishes or a push / pull / clone
  completes while FoxGit is in the background.

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
