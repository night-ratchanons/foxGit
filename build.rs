//! Windows: embed the FoxGit icon and version info into foxgit.exe (Explorer, taskbar, Alt+Tab).

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/foxgit.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon/foxgit.ico")
            .set("ProductName", "FoxGit")
            .set("FileDescription", "FoxGit — Git client")
            .set("LegalCopyright", "© 2026 FoxGit contributors (MIT License)");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the Windows icon: {e}");
        }
    }
}
