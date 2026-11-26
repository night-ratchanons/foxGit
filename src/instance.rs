//! Single instance: a second `foxgit <path>` hands its folders to the running window (as new
//! tabs) and exits. The running app listens on a localhost port written, with a random token, to
//! `<app data>/instance` (readable by the user only).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::OnceLock;
use std::time::Duration;

const MAGIC: &str = "FOXGIT1";

/// Repainted when another instance sends folders (set once the window exists).
pub static CTX: OnceLock<eframe::egui::Context> = OnceLock::new();

pub fn lock_path() -> Option<PathBuf> {
    eframe::storage_dir(crate::APP_NAME).map(|d| d.join("instance"))
}

fn token() -> String {
    let mut s = String::new();
    for i in 0..2u8 {
        let mut h = RandomState::new().build_hasher();
        h.write_u8(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        s.push_str(&format!("{:016x}", h.finish()));
    }
    s
}

/// Give `paths` to the FoxGit that is already running. `true` = it took them (exit now).
pub fn forward(lock: &Path, paths: &[String]) -> bool {
    let Ok(text) = std::fs::read_to_string(lock) else {
        return false;
    };
    let mut it = text.split_whitespace();
    let (Some(port), Some(tok)) = (it.next().and_then(|p| p.parse::<u16>().ok()), it.next()) else {
        return false;
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut msg = format!("{MAGIC} {tok}\n");
    for p in paths {
        let abs = std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
        msg.push_str(&abs.display().to_string().replace('\n', " "));
        msg.push('\n');
    }
    msg.push('\n');
    if s.write_all(msg.as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).is_ok() && reply.trim() == "ok"
}

/// Become the instance others talk to. Paths arrive on the returned channel.
pub fn listen(lock: &Path) -> Option<Receiver<Vec<String>>> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let tok = token();
    if let Some(dir) = lock.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    write_private(lock, &format!("{port} {tok}\n")).ok()?;
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
            let mut r = BufReader::new(match conn.try_clone() {
                Ok(c) => c,
                Err(_) => continue,
            });
            let mut first = String::new();
            if r.read_line(&mut first).is_err() || first.trim() != format!("{MAGIC} {tok}") {
                continue;
            }
            let mut paths = Vec::new();
            loop {
                let mut l = String::new();
                match r.read_line(&mut l) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if l.trim_end_matches(['\n', '\r']).is_empty() => break,
                    Ok(_) => paths.push(l.trim_end_matches(['\n', '\r']).to_string()),
                }
            }
            let mut conn = conn;
            let _ = conn.write_all(b"ok\n");
            if tx.send(paths).is_err() {
                break;
            }
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
        }
    });
    Some(rx)
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_hands_over_paths() {
        let t = tempfile::tempdir().unwrap();
        let lock = t.path().join("instance");
        assert!(!forward(&lock, &["x".into()]), "nobody is listening yet");
        let rx = listen(&lock).unwrap();
        let dir = t.path().join("my repo ไทย");
        std::fs::create_dir(&dir).unwrap();
        assert!(forward(&lock, &[dir.display().to_string()]));
        let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            got,
            [std::fs::canonicalize(&dir).unwrap().display().to_string()]
        );
        // a wrong token is ignored
        let text = std::fs::read_to_string(&lock).unwrap();
        let port = text.split_whitespace().next().unwrap();
        std::fs::write(&lock, format!("{port} nope\n")).unwrap();
        assert!(!forward(&lock, &[]));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }
}
