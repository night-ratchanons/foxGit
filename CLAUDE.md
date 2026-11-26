# RustTree — project guide for Claude Code

A fast Sourcetree-style Git GUI written in Rust. The user speaks Thai — reply in Thai; keep code, identifiers and UI strings in English.

## Commands

```bash
cargo run --release -- ~/path/to/repo [more repos…]   # run (each path opens as a tab)
cargo test                                            # unit + integration tests (create temp git repos)
cargo clippy && cargo fmt                             # keep both clean before finishing a task
RT_BENCH=/path/to/big/repo cargo test --release bench_load -- --ignored --nocapture   # history load benchmark
```

Requires Rust >= 1.95 (eframe 0.37 MSRV), `git` on PATH, a C compiler (libgit2 is vendored).

## Architecture

| File | Responsibility |
|---|---|
| `src/main.rs` | Entry point. First checks `askpass::client_main()` (process started by git as GIT_ASKPASS) before opening any window. |
| `src/app.rs` | `RustTreeApp` state, `RepoTab` (one per open repo), toolbar, dialogs, background job queue, shortcuts, auto-fetch, credential dialog, persistence (`Saved`). |
| `src/tabs.rs` | Browser-style tab strip (drag reorder, context menu), Projects launcher page, project list (`Project`, pinned/recent), repo scanner. |
| `src/views.rs` | Sidebar (branches/remotes/tags/stashes), History view (commit list + graph painting), File Status view (staged/unstaged lists, commit box). |
| `src/diffview.rs` | Virtualized diff viewer + hunk/line staging UI. |
| `src/graph.rs` | Commit-graph lane layout algorithm (pure, unit-tested). |
| `src/git.rs` | Backend. **Reads** via libgit2 (`Repo`: commits, refs, status, diff). **Writes** via the `git` CLI (`Op` enum → `Op::run`). `FileDiff::build_patch` builds partial patches for `git apply`. |
| `src/askpass.rs` | Credential prompts: localhost TCP server + token; our own exe is set as GIT_ASKPASS/SSH_ASKPASS. |
| `src/theme.rs` | Dark (default) / light `Palette`, egui `Style`, and shared painted widgets (`ghost_button`, `segmented`, pills, carets, avatars). Hand-painted UI takes colors from `theme::pal(ui)`. |
| `src/branchtree.rs` | Groups branch names into folders by `/` prefix for the sidebar (pure, unit-tested). |
| `src/github.rs` | GitHub backend: device flow + PAT sign-in, `/user/repos` listing, keychain storage (`keyring`), and `git_credential()` — URL-scoped `-c credential.https://github.com.helper` args injected by `run_git_ext`. |
| `src/account.rs` | GitHub UI: sign-in modal, toolbar account menu, the Projects page "Remote" list with Clone/Open. |

### Key conventions

- **Reads = libgit2, writes = git CLI.** Never write to the repo with git2; add an `Op` variant instead so hooks, signing, SSH and credential helpers behave like the terminal.
- **Adding a git action:** add a variant to `Op` (git.rs) with `title()` and a branch in `Op::run`, then from UI code push `Action::Run(Op::…)`, or `Action::Dialog(Dialog::Confirm { … })` for anything destructive. Jobs run sequentially on a background thread; on completion the tab is refreshed automatically.
- **UI never mutates app state while borrowed:** views collect `Action`s / events into a `Vec` and the app applies them after the frame.
- **Destructive actions always confirm** (discard, delete branch/tag, reset --hard, drop stash).
- **Never block the UI thread** on network or long work. History loads on a thread (`RepoTab::start_history_load`); network ops go through the job queue or the auto-fetch thread.
- Lists and diffs are virtualized with `ScrollArea::show_rows`; keep row heights uniform.
- egui 0.37 API notes: `App::ui(&mut self, ui, frame)` + `App::logic`; panels are `egui::Panel::left/right/top/bottom(id).show(ui, …)` and `CentralPanel::default().show(ui, …)`; use `Id::unique` (not `Id::new`) and `ui.scope_id()` (not `ui.id()`); git2 0.21 string getters return `Result`, not `Option`.
- Fonts: eframe `system_fonts` feature handles Thai/CJK fallback; `egui/monochrome_emoji_fonts` provides icons. Not every Unicode symbol renders (e.g. `⑂` shows as a box on macOS — use `theme::BRANCH` = `⎇`), so check new icons on screen.
- Theme: `theme::apply` sets the theme preference explicitly; never use `ctx.set_visuals` (egui follows the OS appearance and would drop it).

## Testing

- Backend tests in `git.rs` create real temporary repos with the git CLI (`setup()` helper), so new `Op`s and patch logic should get a test there.
- Partial staging must stay byte-exact: CRLF, missing final newline, non-UTF-8 content and paths with spaces/Thai characters are covered by tests and must keep passing.
- After UI changes, run the app and try the change by hand.

## Roadmap (agreed next steps)

1. Conflict resolution view: list conflicted files, "use ours / use theirs", open in external merge tool (`git mergetool`), mark resolved, continue/abort merge or rebase.
2. Single-instance mode + "Open in RustTree" (open a folder as a tab in the running window).
3. File watcher (`notify` crate) to refresh status on save.
4. Multi-select files (Shift/Ctrl+click) for stage/discard.
5. File history + blame.
6. Interactive rebase (reorder / squash / reword).
7. Diff improvements: syntax highlighting, side-by-side, word-level highlight.
8. Settings page (per-repo user.name/email, remotes, external tools) and release packaging (.app / .dmg, GitHub Actions).

## Known limits

- Hunk/line staging only for modified files (new/deleted/conflicted files are staged whole).
- Credentials ticked "Remember" are kept in memory only, for the session. The GitHub account token is the exception: it lives in the OS keychain (only the login is saved in app state).
- GitHub browser sign-in needs an OAuth App client ID (entered in the sign-in dialog or baked in with `RUSTTREE_GITHUB_CLIENT_ID=… cargo build`); a personal access token works without one.
- Bare repositories are not supported.
