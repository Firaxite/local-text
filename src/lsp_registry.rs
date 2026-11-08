// Cross-process LSP coordination — a machine-local, per-user layer that lets
// separate local-text processes (each its own OS window) see and control each
// other's running language servers. Used to cap how many processes run a given
// server (rust-analyzer is RAM-hungry), to locate the project running one, and
// to move/stop a server or raise the window owning it.
//
// Two on-disk artifacts live under the existing per-user runtime dir (the same
// 0700, ownership-checked dir used for SSH ControlMaster sockets):
//
//   <runtime>/lsp/<pid>-<op_id>.json   one tiny entry per running server
//   <runtime>/ctl/p-<pid>.sock         one Unix control socket per process
//
// On Windows (no Unix sockets in std) the control channel is a loopback TCP
// listener instead, and `ctl/p-<pid>.port` holds "<port> <token>": loopback is
// reachable by every local user, so each request must start with the token,
// which only the owner can read from its private runtime dir.
//
// The mechanism is best-effort and self-healing, not a correctness lock: dead
// processes are detected (pid liveness + control connect) and their entries GC'd
// by whichever process next scans, so a crash never permanently holds a slot.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

// ── On-disk entry ───────────────────────────────────────────────────────────
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LspRegistryEntry {
    pub pid:        u32,
    pub op_id:      usize,
    /// Stable language id ("rust"/"typescript"/"python").
    pub lang:       String,
    /// Host tag (`SshHost::display()`); None = local.
    pub ssh_host:   Option<String>,
    /// Workspace root (VPath Display form), for showing "which project".
    pub root:       Option<String>,
    pub started_at: u64,
}

// ── Control protocol (one JSON line each way) ────────────────────────────────
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum CtlRequest {
    Ping,
    StopServer { op_id: usize },
    RaiseWindow,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum CtlResponse {
    Pong { pid: u32 },
    Stopped,
    NotFound,
    Raised,
    Error { msg: String },
}

/// Result of a stop request, returned from the main loop to the accept thread
/// (via the `reply` channel on `UserEvent::LspStopRequested`) and onward to the
/// requesting process.
pub enum StopResult { Stopped, NotFound }

// ── Paths ────────────────────────────────────────────────────────────────────
fn lsp_dir() -> Option<PathBuf> {
    let dir = crate::vpath::control_socket_dir().join("lsp");
    crate::vpath::ensure_private_dir(&dir).ok()?;
    Some(dir)
}

fn ctl_dir() -> Option<PathBuf> {
    let dir = crate::vpath::control_socket_dir().join("ctl");
    crate::vpath::ensure_private_dir(&dir).ok()?;
    Some(dir)
}

fn ctl_socket_path(pid: u32) -> PathBuf {
    let name = if cfg!(windows) { format!("p-{pid}.port") } else { format!("p-{pid}.sock") };
    crate::vpath::control_socket_dir().join("ctl").join(name)
}

// ── Control channel transport ────────────────────────────────────────────────

/// Unix-domain sockets: the 0700 runtime dir is the access control.
#[cfg(unix)]
mod transport {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

    pub type Stream = UnixStream;

    pub struct Listener(UnixListener);

    impl Listener {
        pub fn bind(path: &Path) -> Option<Self> {
            let _ = std::fs::remove_file(path); // clear any stale socket from a prior same-pid run
            UnixListener::bind(path).ok().map(Listener)
        }
        pub fn accept(&self) -> std::io::Result<Stream> { self.0.accept().map(|(s, _)| s) }
    }

    pub fn connect(path: &Path) -> std::io::Result<Stream> { UnixStream::connect(path) }
    /// Read and check the connection's credentials line (none on Unix).
    pub fn authenticate<R: std::io::BufRead>(_reader: &mut R, _path: &Path) -> bool { true }
    pub fn send_credentials<W: std::io::Write>(_w: &mut W, _path: &Path) -> std::io::Result<()> { Ok(()) }
}

/// Loopback TCP + a random token published in the private runtime dir.
#[cfg(not(unix))]
mod transport {
    use std::net::{Ipv4Addr, TcpListener, TcpStream};
    use std::path::Path;
    use std::time::Duration;

    pub type Stream = TcpStream;

    pub struct Listener(TcpListener);

    impl Listener {
        pub fn bind(path: &Path) -> Option<Self> {
            use rand::RngCore;
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
            let port = listener.local_addr().ok()?.port();
            let mut raw = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut raw);
            let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
            // Write atomically so a reader never sees a half-written token.
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, format!("{port} {token}")).ok()?;
            std::fs::rename(&tmp, path).ok()?;
            Some(Listener(listener))
        }
        pub fn accept(&self) -> std::io::Result<Stream> { self.0.accept().map(|(s, _)| s) }
    }

    fn read_endpoint(path: &Path) -> std::io::Result<(u16, String)> {
        let text = std::fs::read_to_string(path)?;
        let (port, token) = text.trim().split_once(' ')
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad endpoint file"))?;
        let port = port.parse::<u16>()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad port"))?;
        Ok((port, token.to_owned()))
    }

    pub fn connect(path: &Path) -> std::io::Result<Stream> {
        let (port, _) = read_endpoint(path)?;
        TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), Duration::from_secs(2))
    }

    /// The first line of every request must be our token.
    pub fn authenticate<R: std::io::BufRead>(reader: &mut R, path: &Path) -> bool {
        let Ok((_, token)) = read_endpoint(path) else { return false };
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 { return false; }
        let got = line.trim().as_bytes();
        // Compare without an early exit so timing doesn't leak the prefix.
        got.len() == token.len()
            && got.iter().zip(token.as_bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }

    pub fn send_credentials<W: std::io::Write>(w: &mut W, path: &Path) -> std::io::Result<()> {
        let (_, token) = read_endpoint(path)?;
        w.write_all(format!("{token}\n").as_bytes())
    }
}

fn entry_path(dir: &std::path::Path, pid: u32, op_id: usize) -> PathBuf {
    dir.join(format!("{pid}-{op_id}.json"))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ── Registry: write / remove ─────────────────────────────────────────────────

/// Record that this process has started an LSP server. Atomic (tmp + rename).
pub fn register(op_id: usize, lang: &str, ssh_host: Option<String>, root: Option<String>) {
    let Some(dir) = lsp_dir() else { return };
    let pid = std::process::id();
    let entry = LspRegistryEntry {
        pid, op_id,
        lang: lang.to_owned(),
        ssh_host, root,
        started_at: now_secs(),
    };
    let Ok(json) = serde_json::to_string(&entry) else { return };
    let tmp = dir.join(format!(".tmp-{pid}-{op_id}"));
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, entry_path(&dir, pid, op_id));
    }
}

/// Remove this process's entry for `op_id`. Idempotent.
pub fn unregister(op_id: usize) {
    if let Some(dir) = lsp_dir() {
        let _ = std::fs::remove_file(entry_path(&dir, std::process::id(), op_id));
    }
}

/// Remove all of this process's registry entries and its control socket. Called
/// on clean exit; the liveness GC is the backstop for unclean exits.
pub fn cleanup_self() {
    let pid = std::process::id();
    if let Some(dir) = lsp_dir() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let prefix = format!("{pid}-");
            for ent in rd.flatten() {
                if ent.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(ent.path());
                }
            }
        }
    }
    let _ = std::fs::remove_file(ctl_socket_path(pid));
}

// ── Discovery + liveness ─────────────────────────────────────────────────────

/// The process exists. All registry entries belong to this user (the runtime dir
/// is private), so a permission error can't hide a live process here.
fn pid_alive(pid: u32) -> bool {
    crate::os::pid_alive(pid)
}

/// A live local-text process holds a connectable control socket. This guards
/// against PID reuse: a recycled pid running some unrelated program won't have
/// our socket. A stale socket file with no listener yields ECONNREFUSED.
fn ctl_socket_responsive(pid: u32) -> bool {
    transport::connect(&ctl_socket_path(pid)).is_ok()
}

fn is_live(entry: &LspRegistryEntry) -> bool {
    if entry.pid == std::process::id() { return true; }
    pid_alive(entry.pid) && ctl_socket_responsive(entry.pid)
}

/// Scan the registry, GC dead entries (and their orphaned sockets), and return
/// the live ones.
pub fn list_live() -> Vec<LspRegistryEntry> {
    let Some(dir) = lsp_dir() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(entry) = serde_json::from_str::<LspRegistryEntry>(&text) else {
            let _ = std::fs::remove_file(&path); // unparseable / partial → drop
            continue;
        };
        if is_live(&entry) {
            out.push(entry);
        } else {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(ctl_socket_path(entry.pid));
        }
    }
    out
}

/// Count live servers matching `lang` and host scope (per (lang, ssh_host), so
/// local and per-remote-host limits are independent).
pub fn count_live(lang: &str, ssh_host: Option<&str>) -> usize {
    list_live().into_iter()
        .filter(|e| e.lang == lang && e.ssh_host.as_deref() == ssh_host)
        .count()
}

// ── Control socket: server side ──────────────────────────────────────────────

/// Owns this process's control socket path; unlinks it on drop (clean exit).
pub struct ControlListener {
    path: PathBuf,
}

impl Drop for ControlListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind this process's control socket and spawn its accept thread. Returns a
/// guard that unlinks the socket on drop. Best-effort: returns None if the
/// runtime dir can't be prepared or the socket can't be bound.
pub fn start_control_listener(proxy: EventLoopProxy<UserEvent>) -> Option<ControlListener> {
    let _ = ctl_dir()?; // ensure dir exists
    let pid = std::process::id();
    let path = ctl_socket_path(pid);
    let listener = transport::Listener::bind(&path)?;
    let conn_path = path.clone();
    thread::spawn(move || {
        loop {
            let Ok(stream) = listener.accept() else { continue };
            let proxy = proxy.clone();
            let path = conn_path.clone();
            thread::spawn(move || handle_conn(stream, &path, &proxy));
        }
    });
    Some(ControlListener { path })
}

fn handle_conn(stream: transport::Stream, path: &std::path::Path, proxy: &EventLoopProxy<UserEvent>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut writer = match stream.try_clone() { Ok(s) => s, Err(_) => return };
    let mut reader = BufReader::new(stream);
    if !transport::authenticate(&mut reader, path) { return; }
    let mut line = String::new();
    // 0 bytes = bare connect (a liveness probe) or EOF — nothing to answer.
    if reader.read_line(&mut line).unwrap_or(0) == 0 { return; }
    let Ok(req) = serde_json::from_str::<CtlRequest>(line.trim()) else { return };
    let resp = match req {
        CtlRequest::Ping => CtlResponse::Pong { pid: std::process::id() },
        CtlRequest::RaiseWindow => {
            let _ = proxy.send_event(UserEvent::RaiseWindowRequested);
            CtlResponse::Raised
        }
        CtlRequest::StopServer { op_id } => {
            let (tx, rx) = mpsc::channel();
            if proxy.send_event(UserEvent::LspStopRequested { op_id, reply: tx }).is_err() {
                CtlResponse::Error { msg: "event loop closed".to_owned() }
            } else {
                match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(StopResult::Stopped)  => CtlResponse::Stopped,
                    Ok(StopResult::NotFound) => CtlResponse::NotFound,
                    Err(_)                   => CtlResponse::Error { msg: "timeout".to_owned() },
                }
            }
        }
    };
    let mut out = serde_json::to_string(&resp).unwrap_or_default();
    out.push('\n');
    let _ = writer.write_all(out.as_bytes());
    let _ = writer.flush();
}

// ── Control socket: client side ──────────────────────────────────────────────

fn send_request(pid: u32, req: &CtlRequest) -> std::io::Result<CtlResponse> {
    let path = ctl_socket_path(pid);
    let stream = transport::connect(&path)?;
    stream.set_read_timeout(Some(Duration::from_secs(6)))?;
    stream.set_write_timeout(Some(Duration::from_secs(6)))?;
    let mut writer = stream.try_clone()?;
    transport::send_credentials(&mut writer, &path)?;
    let mut line = serde_json::to_string(req).unwrap_or_default();
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    serde_json::from_str(resp.trim())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Ask the process owning `victim_pid` to stop its server `op_id`. Blocking —
/// call from a background thread, not the UI thread.
pub fn request_stop(victim_pid: u32, op_id: usize) -> std::io::Result<StopResult> {
    match send_request(victim_pid, &CtlRequest::StopServer { op_id })? {
        CtlResponse::Stopped  => Ok(StopResult::Stopped),
        CtlResponse::NotFound => Ok(StopResult::NotFound),
        CtlResponse::Error { msg } => Err(std::io::Error::other(msg)),
        _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unexpected response")),
    }
}

/// Ask the process `target_pid` to raise its window to the front. Blocking —
/// call from a background thread.
pub fn request_raise(target_pid: u32) -> std::io::Result<()> {
    send_request(target_pid, &CtlRequest::RaiseWindow).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_json_round_trip() {
        let e = LspRegistryEntry {
            pid: 4321, op_id: 7,
            lang: "rust".to_owned(),
            ssh_host: Some("user@host:22".to_owned()),
            root: Some("/home/me/proj".to_owned()),
            started_at: 1_700_000_000,
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: LspRegistryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, e.pid);
        assert_eq!(back.op_id, e.op_id);
        assert_eq!(back.lang, e.lang);
        assert_eq!(back.ssh_host, e.ssh_host);
        assert_eq!(back.root, e.root);
        assert_eq!(back.started_at, e.started_at);
    }

    #[test]
    fn request_kind_round_trip() {
        let r = CtlRequest::StopServer { op_id: 9 };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("StopServer"));
        assert!(matches!(serde_json::from_str::<CtlRequest>(&json).unwrap(),
                         CtlRequest::StopServer { op_id: 9 }));
    }

    #[test]
    fn register_count_unregister_roundtrip() {
        // Unique lang + a high op_id so this never collides with real entries.
        let lang = format!("test-{}", std::process::id());
        let op_id = 990_001;
        unregister(op_id); // clean slate
        assert_eq!(count_live(&lang, None), 0);

        register(op_id, &lang, None, Some("/tmp/proj".to_owned()));
        assert_eq!(count_live(&lang, None), 1);

        let mine: Vec<_> = list_live().into_iter().filter(|e| e.lang == lang).collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].pid, std::process::id()); // our own entry is always live
        assert_eq!(mine[0].root.as_deref(), Some("/tmp/proj"));

        // Host scoping: a different host scope shouldn't match.
        assert_eq!(count_live(&lang, Some("user@host")), 0);

        unregister(op_id);
        assert_eq!(count_live(&lang, None), 0);
    }
}
