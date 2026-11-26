//! Credential prompts for git/ssh ("askpass").
//!
//! When git or ssh needs a username, password, token or key passphrase, it runs the program in
//! `GIT_ASKPASS` / `SSH_ASKPASS` with the prompt as its argument and reads the answer from stdout.
//! We point both at our own executable. Started that way, RustTree does not open a window: it
//! connects to the running app over a localhost socket (protected by a random token), the app
//! shows a dialog, and the answer is printed back to git.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;
use std::time::Duration;

const ENV_PORT: &str = "RUSTTREE_ASKPASS_PORT";
const ENV_TOKEN: &str = "RUSTTREE_ASKPASS_TOKEN";
const ENV_MODE: &str = "RUSTTREE_ASKPASS_MODE";

struct Server {
    exe: String,
    port: u16,
    token: String,
}

static SERVER: OnceLock<Server> = OnceLock::new();

/// A prompt waiting for an answer from the UI.
pub struct AskRequest {
    pub prompt: String,
    pub background: bool,
    reply: Sender<Option<String>>,
}

impl AskRequest {
    pub fn answer(self, value: Option<String>) {
        let _ = self.reply.send(value);
    }

    /// Passwords, tokens and passphrases are typed into a masked field.
    pub fn is_secret(&self) -> bool {
        let p = self.prompt.to_lowercase();
        ["password", "passphrase", "token", "pin", "secret"]
            .iter()
            .any(|k| p.contains(k))
    }
}

/// Environment variables for git child processes.
pub fn child_env(background: bool) -> Vec<(&'static str, String)> {
    let Some(s) = SERVER.get() else {
        return Vec::new();
    };
    vec![
        ("GIT_ASKPASS", s.exe.clone()),
        ("SSH_ASKPASS", s.exe.clone()),
        // OpenSSH >= 8.4: use SSH_ASKPASS even without a DISPLAY / when a tty exists
        ("SSH_ASKPASS_REQUIRE", "force".into()),
        (ENV_PORT, s.port.to_string()),
        (ENV_TOKEN, s.token.clone()),
        (
            ENV_MODE,
            if background {
                "background"
            } else {
                "interactive"
            }
            .into(),
        ),
    ]
}

fn random_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..2u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Start the prompt server. Returns the channel the UI polls for prompts.
pub fn start(on_request: impl Fn() + Send + 'static) -> Option<Receiver<AskRequest>> {
    let exe = std::env::current_exe().ok()?.to_string_lossy().to_string();
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let token = random_token();
    let (tx, rx) = channel::<AskRequest>();
    let expected = token.clone();
    SERVER.get_or_init(|| Server { exe, port, token });

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            let expected = expected.clone();
            let notify = &on_request;
            if handle(stream, &expected, &tx).is_some() {
                notify();
            }
        }
    });
    Some(rx)
}

/// Reads one request and hands the connection to a waiting thread. Returns Some if a request was queued.
fn handle(stream: TcpStream, token: &str, tx: &Sender<AskRequest>) -> Option<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut got_token = String::new();
    reader.read_line(&mut got_token).ok()?;
    if got_token.trim_end() != token {
        return None;
    }
    let mut mode = String::new();
    reader.read_line(&mut mode).ok()?;
    let mut prompt = String::new();
    reader.read_to_string(&mut prompt).ok()?;

    let (reply_tx, reply_rx) = channel();
    tx.send(AskRequest {
        prompt: prompt.trim().to_string(),
        background: mode.trim() == "background",
        reply: reply_tx,
    })
    .ok()?;

    std::thread::spawn(move || {
        let mut stream = stream;
        // Give the user up to 10 minutes to answer.
        let answer = reply_rx
            .recv_timeout(Duration::from_secs(600))
            .ok()
            .flatten();
        let msg = match answer {
            Some(a) => format!("OK\n{a}"),
            None => "CANCEL\n".to_string(),
        };
        let _ = stream.write_all(msg.as_bytes());
    });
    Some(())
}

/// If this process was started by git/ssh as an askpass helper, answer the prompt and
/// return the exit code. Returns `None` for a normal GUI start.
pub fn client_main() -> Option<i32> {
    let port: u16 = std::env::var(ENV_PORT).ok()?.parse().ok()?;
    let token = std::env::var(ENV_TOKEN).ok()?;
    let mode = std::env::var(ENV_MODE).unwrap_or_else(|_| "interactive".into());
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Password:".into());

    let run = || -> std::io::Result<Option<String>> {
        let mut s = TcpStream::connect(("127.0.0.1", port))?;
        s.write_all(format!("{token}\n{mode}\n{prompt}").as_bytes())?;
        s.shutdown(std::net::Shutdown::Write)?;
        let mut resp = String::new();
        s.read_to_string(&mut resp)?;
        Ok(resp.strip_prefix("OK\n").map(|a| a.to_string()))
    };
    match run() {
        Ok(Some(answer)) => {
            println!("{answer}");
            Some(0)
        }
        _ => Some(1),
    }
}
