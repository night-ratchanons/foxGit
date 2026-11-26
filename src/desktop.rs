//! Desktop integration: finding programs on the login-shell PATH, the code editor (open a file at
//! a line), the system terminal app, and OS notifications / dock attention.

use crate::shellenv;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// Editors with a command-line launcher, in the order they are tried.
pub const EDITORS: [(&str, &str); 4] = [
    ("Cursor", "cursor"),
    ("VS Code", "code"),
    ("Zed", "zed"),
    ("Sublime Text", "subl"),
];

static FOUND: OnceLock<Vec<(&'static str, PathBuf)>> = OnceLock::new();
/// The editor picked in Settings (`None` = the first one found).
static PREFERRED: Mutex<Option<String>> = Mutex::new(None);

pub fn set_preferred_editor(name: Option<String>) {
    if let Ok(mut p) = PREFERRED.lock() {
        *p = name.filter(|n| !n.is_empty());
    }
}

/// Installed editors (looked up once the login-shell PATH is known).
pub fn installed_editors() -> Vec<(&'static str, PathBuf)> {
    let scan = || {
        EDITORS
            .iter()
            .filter_map(|(name, prog)| which(prog).map(|p| (*name, p)))
            .collect::<Vec<_>>()
    };
    if shellenv::ready() {
        FOUND.get_or_init(scan).clone()
    } else {
        scan()
    }
}

/// The editor to use: the preferred one when installed, else the first found.
pub fn editor() -> Option<(&'static str, PathBuf)> {
    let found = installed_editors();
    let pref = PREFERRED.lock().ok().and_then(|p| p.clone());
    pref.and_then(|p| found.iter().find(|(n, _)| *n == p).cloned())
        .or_else(|| found.into_iter().next())
}

/// Open `file` (at `line`, 1-based) in the code editor, with `folder` as the project.
/// Falls back to the system's default app for the file.
pub fn open_in_editor(
    folder: &Path,
    file: Option<&Path>,
    line: Option<usize>,
) -> Result<String, String> {
    if let Some((name, exe)) = editor() {
        let mut args: Vec<std::ffi::OsString> = Vec::new();
        match (name, file) {
            ("Cursor" | "VS Code", Some(f)) => {
                args.push(folder.into());
                args.push("-g".into());
                args.push(with_line(f, line).into());
            }
            (_, Some(f)) => args.push(with_line(f, line).into()),
            (_, None) => args.push(folder.into()),
        }
        let refs: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        return spawn_detached(&exe, &refs)
            .map(|_| name.to_string())
            .map_err(|e| format!("Could not start {name}: {e}"));
    }
    let target = file.unwrap_or(folder);
    open_with_system(target).map(|_| "the default app".to_string())
}

fn with_line(f: &Path, line: Option<usize>) -> String {
    match line {
        Some(l) => format!("{}:{l}", f.display()),
        None => f.display().to_string(),
    }
}

/// Open a file or folder with the OS default handler.
pub fn open_with_system(target: &Path) -> Result<(), String> {
    let t = target.as_os_str();
    let r = if cfg!(target_os = "macos") {
        Command::new("open").arg(t).spawn()
    } else if cfg!(windows) {
        Command::new("cmd")
            .args(["/c".as_ref(), "start".as_ref(), "".as_ref(), t])
            .spawn()
    } else {
        Command::new("xdg-open").arg(t).spawn()
    };
    r.map(|_| ())
        .map_err(|e| format!("Could not open {}: {e}", target.display()))
}

pub fn spawn_detached(exe: &Path, args: &[&std::ffi::OsStr]) -> std::io::Result<()> {
    let mut c = Command::new(exe);
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(p) = shellenv::path() {
        c.env("PATH", p);
    }
    c.spawn().map(|_| ())
}

/// Open the system's terminal app in `dir`.
pub fn open_terminal_app(dir: &Path) -> Result<(), String> {
    let d = dir.as_os_str();
    let try_run = |prog: &str, args: &[&std::ffi::OsStr]| -> bool {
        Command::new(prog)
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    };
    let ok = if cfg!(target_os = "macos") {
        try_run("open", &["-a".as_ref(), "Terminal".as_ref(), d])
    } else if cfg!(windows) {
        try_run("wt", &["-d".as_ref(), d])
            || try_run("cmd", &["/c".as_ref(), "start".as_ref(), "cmd".as_ref()])
    } else {
        let wd = format!("--working-directory={}", dir.display());
        try_run("x-terminal-emulator", &[])
            || try_run("gnome-terminal", &[wd.as_ref()])
            || try_run("konsole", &["--workdir".as_ref(), d])
            || try_run("xfce4-terminal", &[wd.as_ref()])
            || try_run("kitty", &["--directory".as_ref(), d])
            || try_run("alacritty", &["--working-directory".as_ref(), d])
            || try_run("xterm", &[])
    };
    if ok {
        Ok(())
    } else {
        Err("Could not find a terminal app to open".into())
    }
}

pub fn which(program: &str) -> Option<PathBuf> {
    let path = shellenv::path()
        .map(String::from)
        .or_else(|| std::env::var("PATH").ok())?;
    let (sep, exts): (char, &[&str]) = if cfg!(windows) {
        (';', &[".exe", ".cmd", ".bat", ""])
    } else {
        (':', &[""])
    };
    path.split(sep)
        .filter(|d| !d.is_empty())
        .flat_map(|d| {
            exts.iter()
                .map(move |e| Path::new(d).join(format!("{program}{e}")))
        })
        .find(|p| p.is_file())
}

/// A system notification (Notification Center / libnotify / a Windows toast). Best effort.
pub fn notify(title: &str, body: &str) {
    let (title, body) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        let _ = if cfg!(target_os = "macos") {
            let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
            Command::new("osascript")
                .args([
                    "-e",
                    &format!(
                        "display notification \"{}\" with title \"{}\"",
                        q(&body),
                        q(&title)
                    ),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        } else if cfg!(windows) {
            let q = |s: &str| s.replace('\'', "''");
            let script = format!(
                "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
                 $t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
                 $x = $t.GetElementsByTagName('text'); $x.Item(0).AppendChild($t.CreateTextNode('{}')) > $null; $x.Item(1).AppendChild($t.CreateTextNode('{}')) > $null; \
                 [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('FoxGit').Show([Windows.UI.Notifications.ToastNotification]::new($t))",
                q(&title),
                q(&body)
            );
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script]);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.creation_flags(0x0800_0000);
            }
            c.stdout(Stdio::null()).stderr(Stdio::null()).status()
        } else {
            Command::new("notify-send")
                .args(["-a", "FoxGit", &title, &body])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };
    });
}
