# FoxGit — project guide for Claude Code

A fast Sourcetree-style Git GUI written in Rust (formerly "RustTree": `main::migrate_storage` and `github::load_token` carry old saved state / keychain tokens over; `STORAGE_KEY` keeps its old value on purpose). The user speaks Thai — reply in Thai; keep code, identifiers and UI strings in English.

## Commands

```bash
cargo run --release -- ~/path/to/repo [more repos…]   # run (each path opens as a tab)
cargo test                                            # unit + integration tests (create temp git repos)
cargo clippy && cargo fmt                             # keep both clean before finishing a task
RT_BENCH=/path/to/big/repo cargo test --release bench_load -- --ignored --nocapture   # history load benchmark
scripts/bundle-macos.sh [--universal] [--dmg]         # build target/release/FoxGit.app (+ .dmg)
scripts/make-icons.sh                                 # re-render assets/icon/* (.png/.icns/.ico) from foxgit.svg (needs resvg)
scripts/demo-repos.sh                                 # throw-away demo repos in /tmp (screenshots / manual testing)
python3 scripts/third-party-licenses.py > THIRD-PARTY-LICENSES.txt   # after dependency changes
```

Requires Rust >= 1.95 (eframe 0.37 MSRV), `git` on PATH, a C compiler (libgit2 is vendored).

## Architecture

| File | Responsibility |
|---|---|
| `src/main.rs` | Entry point. First checks `askpass::client_main()` (process started by git as GIT_ASKPASS) before opening any window. |
| `src/app.rs` | `FoxGitApp` state, `RepoTab` (one per open repo), toolbar, dialogs, background job queue, shortcuts, auto-fetch, credential dialog, persistence (`Saved`). |
| `src/tabs.rs` | Browser-style tab strip (drag reorder, context menu), Projects launcher page, project list (`Project`, pinned/recent), repo scanner. |
| `src/views.rs` | Activity rail (Changes / History / GitHub / Search / Agents modes = `View`), mode sidebar (branches/remotes/tags/stashes, or the agent chats), History view (commit list + graph painting), File Status view (staged/unstaged lists, commit box). |
| `src/diffview.rs` | Virtualized diff viewer + hunk/line staging UI. |
| `src/graph.rs` | Commit-graph lane layout algorithm (pure, unit-tested). |
| `src/git.rs` | Backend. **Reads** via libgit2 (`Repo`: commits, refs, status, diff). **Writes** via the `git` CLI (`Op` enum → `Op::run`). `FileDiff::build_patch` builds partial patches for `git apply`. |
| `src/askpass.rs` | Credential prompts: localhost TCP server + token; our own exe is set as GIT_ASKPASS/SSH_ASKPASS. |
| `src/theme.rs` | Dark (default) / light `Palette`, egui `Style`, and shared painted widgets (`ghost_button`, `segmented`, pills, carets, avatars). Hand-painted UI takes colors from `theme::pal(ui)`. |
| `src/branchtree.rs` | Groups branch names into folders by `/` prefix for the sidebar (pure, unit-tested). |
| `src/github.rs` | GitHub backend: personal-access-token sign-in (the only sign-in method, by choice; several accounts), `/user/repos` listing, org list + repo creation, keychain storage (`keyring`), and `git_credential()` — URL-scoped `-c credential.https://github.com.helper` args injected by `run_git_ext`; the inline helper picks the token by the user name in the remote URL (`https://login@github.com/…`), else the active account (`FOXGIT_GH_USER_0`). |
| `src/account.rs` | GitHub UI: sign-in modal, toolbar account menu, the Projects page "Remote" list with Clone/Open. |
| `src/agents.rs` | AI agent chats (Claude Code `claude -p --output-format stream-json`, Codex `codex exec --json`, Hermes `hermes chat --format stream-json`): one git worktree + `agent/<slug>` branch per chat under `<app data>/worktrees`, CLI run headless per message (resuming the agent's session), JSON lines parsed into chat lines (`parse_line`, unit-tested with real Claude/Hermes lines; Codex shapes untested — not installed here), Review / Merge (`Op::AgentMerge`) / Discard (`Op::AgentCleanup`). |
| `src/terminal.rs` | Embedded terminal: `portable-pty` shell (or agent TUI) + `vt100` parser painted as a cell grid (256/true colour, cursor, scrollback, focus-locked keyboard → escape sequences). Used by the agent chats' Terminal tab. |
| `src/shellenv.rs` | Login-shell `PATH` for git child processes (apps started from Finder get a minimal PATH) + `git --version` check. |
| `src/update.rs` | Update check against GitHub Releases of `FOXGIT_REPO` / `GITHUB_REPOSITORY` (set automatically in CI builds; dev builds don't check). |
| `src/hub.rs` | GitHub mode (`View::GitHub`): PR / issue lists + detail (conversation, files, checks), merge / review / comment / close, Checkout PR (`Op::CheckoutPr` → local `pr/<n>`, no upstream), New pull request form (`HubAction::PushThenCreate` → push job → `AfterJob::CreatePr` → API), CI checks of a commit (cached per SHA, pending ones re-polled). All API calls on threads, results over a channel (`Hub::poll`). The app builds a `RepoInfo` per frame (`FoxGitApp::hub_info`: GitHub remote, token for its URL, branch). |
| `src/conflict.rs` | Conflict markers → `Chunk`s (pure, unit-tested, byte-exact incl. CRLF / diff3 / Thai), per-block picks, `render`; the File Status view shows it for a conflicted file. Banner with Continue / Abort (`Op::Continue/Abort(Sequence)`). |
| `src/runner.rs` | Run panel: task detection (package.json + lockfile → pnpm/yarn/bun/npm, Cargo, Makefile targets, Go), terminals docked at the bottom of a tab (`RepoTab::run`), localhost URL detection. |
| `src/search.rs` | Search mode: `git grep -n -z --column` on a thread, virtualized grouped results, click → `desktop::open_in_editor` at the line. |
| `src/palette.rs` | Command palette (`Cmd` enum + fuzzy `score`); `app/commands.rs` lists the items and runs them. |
| `src/editor.rs` | Quick-edit window for a working-copy file (UTF-8, < 2 MB). |
| `src/aicommit.rs` | ✨ commit message: staged diff + recent subjects → `claude -p --output-format text --model haiku`. |
| `src/desktop.rs` | `which` on the login PATH, editor detection / preference (`open_in_editor` with line), Terminal app, OS notifications. |
| `src/instance.rs` | Single instance: localhost port + random token in `<app data>/instance`; a second `foxgit <path>` forwards and exits. Set `FOXGIT_NEW_INSTANCE=1` to force a separate window (e.g. `cargo run` while the installed FoxGit is open). |
| `src/app/` | Child modules of `app.rs` with access to `FoxGitApp`: `settings.rs` (Settings window), `commands.rs` (palette items / commands, plus the debug-only `FOXGIT_DEMO` hook), `dashboard.rs` (Projects → Dashboard). |

Release: `.github/workflows/release.yml` builds macOS (universal .dmg), Windows (.zip, icon via `build.rs`), Linux (.tar.gz + .deb) on a `v*` tag and makes a draft GitHub Release from the `CHANGELOG.md` section. See `docs/RELEASING.md`. CI (`ci.yml`) runs fmt, `clippy -D warnings` and tests on all three OSes.

### Key conventions

- **Reads = libgit2, writes = git CLI.** Never write to the repo with git2; add an `Op` variant instead so hooks, signing, SSH and credential helpers behave like the terminal.
- **Adding a git action:** add a variant to `Op` (git.rs) with `title()` and a branch in `Op::run`, then from UI code push `Action::Run(Op::…)`, or `Action::Dialog(Dialog::Confirm { … })` for anything destructive. Jobs run sequentially on a background thread; on completion the tab is refreshed automatically.
- **UI never mutates app state while borrowed:** views collect `Action`s / events into a `Vec` and the app applies them after the frame.
- **Destructive actions always confirm** (discard, delete branch/tag, reset --hard, drop stash).
- **Never block the UI thread** on network or long work. History loads on a thread (`RepoTab::start_history_load`); network ops go through the job queue or the auto-fetch thread.
- Lists and diffs are virtualized with `ScrollArea::show_rows`; keep row heights uniform.
- Shortcut labels go through `theme::shortcut("T")` (⌘ on macOS, Ctrl elsewhere).
- Screenshots in `docs/screenshots/` come from `scripts/demo-repos.sh` — never from real projects.
- egui 0.37 API notes: `App::ui(&mut self, ui, frame)` + `App::logic`; panels are `egui::Panel::left/right/top/bottom(id).show(ui, …)` and `CentralPanel::default().show(ui, …)`; use `Id::unique` (not `Id::new`) and `ui.scope_id()` (not `ui.id()`); git2 0.21 string getters return `Result`, not `Option`.
- Fonts: eframe `system_fonts` feature handles Thai/CJK fallback; `egui/monochrome_emoji_fonts` provides icons. Not every Unicode symbol renders (e.g. `⑂` shows as a box on macOS — use `theme::BRANCH` = `⎇`), so check new icons on screen.
- Theme: `theme::apply` sets the theme preference explicitly; never use `ctx.set_visuals` (egui follows the OS appearance and would drop it).

## Testing

- Backend tests in `git.rs` create real temporary repos with the git CLI (`setup()` helper), so new `Op`s and patch logic should get a test there.
- Partial staging must stay byte-exact: CRLF, missing final newline, non-UTF-8 content and paths with spaces/Thai characters are covered by tests and must keep passing.
- After UI changes, run the app and try the change by hand. For screenshots without driving the UI, debug builds read `FOXGIT_DEMO` (e.g. `changes`, `search:price`, `run:dev`, `palette`, `settings`, `dashboard`, `hubfake[:files|checks]` = made-up PRs, no network); run with an isolated `HOME` and `FOXGIT_NEW_INSTANCE=1`. `scripts/demo-repos.sh` includes `web-app` (package.json scripts + a merge stopped on a conflict).
- `cargo test gh_live -- --ignored` checks that real (public, unauthenticated) GitHub responses still parse.

## Roadmap (agreed next steps)

Done: GitHub PRs / issues / CI, conflict view, run panel, search, palette, settings, dashboard, single instance, AI commit message.

1. Markdown rendering for PR / issue text; inline review comments on PR files.
2. Finder "Open With" / Dock icon drop (Apple Events — winit doesn't expose them yet).
3. File watcher (`notify` crate) to refresh status on save.
4. Multi-select files (Shift/Ctrl+click) for stage/discard.
5. File history + blame.
6. Interactive rebase (reorder / squash / reword).
7. Diff improvements: syntax highlighting, side-by-side, word-level highlight.
8. Packaging: Windows code signing, an installer (MSI), Linux AppImage / arm64.

## Known limits

- Hunk/line staging only for modified files (new/deleted/conflicted files are staged whole).
- Credentials ticked "Remember" are kept in memory only, for the session. The GitHub account token is the exception: it lives in the OS keychain (only the login is saved in app state).
- Bare repositories are not supported.
