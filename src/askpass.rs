// SSH password / passphrase / host-key prompts.
//
// `ssh` runs without a terminal, so it can't ask the user anything itself. Instead
// it is pointed at this program as its SSH_ASKPASS helper: ssh runs
// `local-text "<prompt>"` with LOCAL_TEXT_ASKPASS set, and that process (see
// `helper_main`, checked first thing in `main`) forwards the prompt over a loopback
// TCP connection to the running editor, which shows it in a modal prompt and sends
// the answer back for the helper to print. The connection carries a random token
// that only our own ssh children know, so other local programs can't use the
// listener to pop up fake prompts.
//
// With ControlMaster multiplexing (macOS/Linux) only the master connection
// authenticates, so the user is asked once per host. Windows' OpenSSH can't
// multiplex — every command is a new connection — so there answers to secret
// prompts are remembered (in memory, for this run) and reused; a failed
// authentication forgets them.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

const ENV_ADDR:  &str = "LOCAL_TEXT_ASKPASS";
const ENV_TOKEN: &str = "LOCAL_TEXT_ASKPASS_TOKEN";

/// What kind of answer a prompt wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptKind {
    /// A password or passphrase (typed text is hidden).
    Secret,
    /// A visible answer (e.g. "yes" to accept a new host key).
    Plain,
    /// Just OK / Cancel (SSH_ASKPASS_PROMPT=confirm).
    Confirm,
}

struct Server {
    addr:  String,
    token: String,
    /// Remembered secret answers, by prompt (Windows only, see the module docs).
    cache: Mutex<HashMap<String, String>>,
}

static SERVER: OnceLock<Server> = OnceLock::new();

/// Start listening for prompts from helper processes, showing them in the editor.
/// Idempotent.
pub fn start(proxy: EventLoopProxy<UserEvent>) {
    start_with(move |prompt, kind| {
        let (tx, rx) = mpsc::sync_channel(1);
        proxy.send_event(UserEvent::AskPass { prompt, kind, reply: tx }).ok()?;
        rx.recv_timeout(Duration::from_secs(600)).ok().flatten()
    });
}

type Asker = dyn Fn(String, PromptKind) -> Option<String> + Send + Sync;

/// Start the listener with `ask` answering prompts (blocking until the user does).
fn start_with(ask: impl Fn(String, PromptKind) -> Option<String> + Send + Sync + 'static) {
    if SERVER.get().is_some() { return; }
    let ask: std::sync::Arc<Asker> = std::sync::Arc::new(ask);
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else { return };
    let Ok(addr) = listener.local_addr() else { return };
    let mut raw = [0u8; 24];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut raw);
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let _ = SERVER.set(Server { addr: addr.to_string(), token, cache: Mutex::new(HashMap::new()) });
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let ask = std::sync::Arc::clone(&ask);
            std::thread::spawn(move || { let _ = serve(stream, &*ask); });
        }
    });
}

/// Point an `ssh` command at our helper (no-op if the server isn't running).
pub fn configure(cmd: &mut Command) {
    let Some(srv) = SERVER.get() else { return };
    let Ok(exe) = std::env::current_exe() else { return };
    cmd.env("SSH_ASKPASS", exe)
       .env("SSH_ASKPASS_REQUIRE", "force")
       .env(ENV_ADDR, &srv.addr)
       .env(ENV_TOKEN, &srv.token);
    // OpenSSH before 8.4 ignores SSH_ASKPASS_REQUIRE and only uses the helper when
    // DISPLAY is set and there is no terminal.
    if std::env::var_os("DISPLAY").is_none() { cmd.env("DISPLAY", "local-text:0"); }
}

/// Forget remembered answers (after an authentication failure).
pub fn forget_cached() {
    if let Some(srv) = SERVER.get() {
        srv.cache.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

fn token_matches(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn serve(stream: TcpStream, ask: &Asker) -> std::io::Result<()> {
    let Some(srv) = SERVER.get() else { return Ok(()) };
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new((&stream).take(64 * 1024)).read_line(&mut line)?;
    let req: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    if !token_matches(req["token"].as_str().unwrap_or(""), &srv.token) { return Ok(()); }
    let prompt: String = req["prompt"].as_str().unwrap_or("").chars().take(2000).collect();
    let kind = match req["kind"].as_str() {
        Some("confirm") => PromptKind::Confirm,
        Some("plain") => PromptKind::Plain,
        _ => PromptKind::Secret,
    };
    let cache_it = kind == PromptKind::Secret && !cfg!(unix);
    let cached = cache_it.then(|| srv.cache.lock().unwrap_or_else(|p| p.into_inner()).get(&prompt).cloned()).flatten();
    let answer = match cached {
        Some(a) => Some(a),
        None => ask(prompt.clone(), kind),
    };
    if let (true, Some(a)) = (cache_it, &answer) {
        srv.cache.lock().unwrap_or_else(|p| p.into_inner()).insert(prompt, a.clone());
    }
    let reply = match answer {
        Some(a) => serde_json::json!({ "ok": true, "answer": a }),
        None => serde_json::json!({ "ok": false }),
    };
    let mut w = &stream;
    w.write_all(format!("{reply}\n").as_bytes())?;
    w.flush()
}

/// If this process was started by ssh as its askpass helper, ask the editor and
/// return the exit code; otherwise None (start the editor normally).
pub fn helper_main() -> Option<i32> {
    let addr = std::env::var(ENV_ADDR).ok()?;
    let token = std::env::var(ENV_TOKEN).unwrap_or_default();
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let kind = match std::env::var("SSH_ASKPASS_PROMPT").as_deref() {
        Ok("confirm") | Ok("none") => "confirm",
        _ if prompt.contains("(yes/no") => "plain",
        _ => "secret",
    };
    match ask_editor(&addr, &token, &prompt, kind) {
        Some(answer) => {
            println!("{answer}");
            Some(0)
        }
        None => Some(1),
    }
}

/// Send one prompt to the editor's listener and wait for the answer.
fn ask_editor(addr: &str, token: &str, prompt: &str, kind: &str) -> Option<String> {
    let sock_addr = addr.parse().ok()?;
    let stream = TcpStream::connect_timeout(&sock_addr, Duration::from_secs(5)).ok()?;
    let req = serde_json::json!({ "token": token, "prompt": prompt, "kind": kind });
    (&stream).write_all(format!("{req}\n").as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    if v["ok"].as_bool() != Some(true) { return None; }
    Some(v["answer"].as_str().unwrap_or("").to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_round_trip_and_need_the_token() {
        start_with(|prompt, kind| match kind {
            PromptKind::Secret => Some(format!("pw for {prompt}")),
            PromptKind::Plain => Some("yes".to_owned()),
            PromptKind::Confirm => None,
        });
        let srv = SERVER.get().unwrap();
        assert_eq!(ask_editor(&srv.addr, &srv.token, "me@host's password:", "secret").as_deref(),
                   Some("pw for me@host's password:"));
        assert_eq!(ask_editor(&srv.addr, &srv.token, "Continue (yes/no)?", "plain").as_deref(), Some("yes"));
        assert_eq!(ask_editor(&srv.addr, &srv.token, "Allow?", "confirm"), None);
        // A caller without the token gets nothing (the connection is just closed).
        assert_eq!(ask_editor(&srv.addr, "wrong", "me@host's password:", "secret"), None);
    }

    #[test]
    fn token_comparison_is_exact() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", "abcd"));
        assert!(!token_matches("", "a"));
    }
}
