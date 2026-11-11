// Language Server Protocol client.
//
// LspManager holds one LspServer per language.  Servers are started lazily when
// a file of the corresponding language is opened.  Each server gets its own
// OutputPane for its stdout/stderr log.
//
// JSON-RPC framing uses the LSP Content-Length header format.
// A background thread reads messages from the server's stdout and sends them
// to the main loop via EventLoopProxy<UserEvent>.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use winit::event_loop::EventLoopProxy;

use crate::ssh;
use crate::vpath::{SshHost, VPath};
use crate::{Diagnostic, DiagSeverity, Lang, UserEvent};

/// Hard cap on a single LSP message body. A legitimate JSON-RPC message is never
/// remotely this large; without it a buggy or hostile server can force an unbounded
/// `vec![0u8; n]` allocation via the Content-Length header (OOM/abort). Mirrors the
/// 64 MiB frame cap in `collab::read_frame`.
const MAX_LSP_FRAME: usize = 64 << 20; // 64 MiB

// ── PendingKind ───────────────────────────────────────────────────────────────
pub enum PendingKind {
    Definition,
    References,
    /// `generation`: the buffer's edit generation the request was computed for
    /// (stale results are dropped). `save_after`: format-on-save — write the
    /// file again once the edits are applied.
    Formatting { path: VPath, generation: u64, save_after: bool },
    /// `then_format`: format-on-save also wants formatting; request it after the
    /// import edits land (both can't be computed against the same version).
    OrganizeImports { path: VPath, generation: u64, then_format: bool, save_after: bool },
    /// `anchor`: char offset where the word being completed starts.
    Completion { path: VPath, anchor: usize },
    /// `pos`: char offset the hover was requested for.
    Hover { path: VPath, pos: usize },
    SignatureHelp { path: VPath },
    Rename { path: VPath, generation: u64 },
}

// ── LspLaunchOverrides ────────────────────────────────────────────────────────
/// Per-project launch overrides loaded from `.vscode/local-text.json`. Empty by
/// default (no override → the hardcoded binary/args/inherited env are used).
#[derive(Default, Clone)]
pub struct LspLaunchOverrides {
    /// Override the server binary (path or name). `None` = use the default.
    pub command: Option<String>,
    /// Override the launch arguments. `None` = use the default.
    pub args:    Option<Vec<String>>,
    /// Extra environment variables to set on the server process (merged over the
    /// inherited environment).
    pub env:     Vec<(String, String)>,
}

// ── Outgoing message queue ────────────────────────────────────────────────────
//
// Messages to the server are written by a dedicated thread, never by the UI
// thread: a server that stops reading (busy indexing, wedged, or a stalled SSH
// link) would otherwise fill the pipe and freeze the editor inside write().
// Because document sync is full-text, a newer didChange for the same document
// supersedes an older one still waiting in the queue, so a stuck server costs at
// most one pending copy of each document rather than one per keystroke.

enum Outgoing {
    Raw(String),
    DocChange { uri: String, body: String },
}

struct OutQueue {
    items:  VecDeque<Outgoing>,
    closed: bool,
}

pub struct LspWriter {
    queue: Arc<(Mutex<OutQueue>, Condvar)>,
}

impl LspWriter {
    fn spawn(mut stdin: ChildStdin) -> Self {
        let queue = Arc::new((Mutex::new(OutQueue { items: VecDeque::new(), closed: false }), Condvar::new()));
        let q = Arc::clone(&queue);
        thread::spawn(move || loop {
            let next = {
                let (lock, cvar) = &*q;
                let mut st = lock.lock().unwrap_or_else(|p| p.into_inner());
                while st.items.is_empty() && !st.closed {
                    st = cvar.wait(st).unwrap_or_else(|p| p.into_inner());
                }
                match st.items.pop_front() {
                    Some(item) => item,
                    None => break, // closed and drained
                }
            };
            let body = match next { Outgoing::Raw(b) | Outgoing::DocChange { body: b, .. } => b };
            if write_message(&mut stdin, &body).is_err() { break; }
        });
        LspWriter { queue }
    }

    fn push(&self, item: Outgoing) {
        let (lock, cvar) = &*self.queue;
        let mut st = lock.lock().unwrap_or_else(|p| p.into_inner());
        if enqueue(&mut st, item) { cvar.notify_one(); }
    }
}

/// Add `item` to the queue; returns true if a new entry was appended. A didChange
/// replaces a still-queued change for the same document, but only when it is the
/// last entry — so it never jumps ahead of a request queued after it.
fn enqueue(st: &mut OutQueue, item: Outgoing) -> bool {
    if st.closed { return false; }
    if let Outgoing::DocChange { uri, body } = &item {
        if let Some(Outgoing::DocChange { uri: last_uri, body: last_body }) = st.items.back_mut() {
            if last_uri == uri {
                *last_body = body.clone();
                return false;
            }
        }
    }
    st.items.push_back(item);
    true
}

impl Drop for LspWriter {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.queue;
        lock.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
        cvar.notify_one();
    }
}

// ── LspServer ─────────────────────────────────────────────────────────────────
pub struct LspServer {
    pub lang:           Lang,
    pub process:        Child,
    writer:             LspWriter,
    pub output_pane_id: usize,
    pub request_id:     u64,
    pub doc_version:    HashMap<VPath, u64>,
    pub initialized:    bool,
    pub pending:        HashMap<u64, PendingKind>,
    /// If Some, this server runs on a remote host via SSH.
    pub ssh_host:       Option<SshHost>,
    /// From the initialize response: characters that trigger completion / signature
    /// help, and which optional features the server offers.
    pub caps:           ServerCaps,
}

#[derive(Default, Clone, Debug)]
pub struct ServerCaps {
    pub completion:          bool,
    pub completion_triggers: Vec<char>,
    pub hover:               bool,
    pub signature_help:      bool,
    pub signature_triggers:  Vec<char>,
    pub rename:              bool,
}

impl ServerCaps {
    /// Read the capabilities out of an initialize response message.
    pub fn from_initialize_response(json: &str) -> ServerCaps {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return ServerCaps::default() };
        let c = &v["result"]["capabilities"];
        let chars = |a: &serde_json::Value| -> Vec<char> {
            a.as_array().map(|a| a.iter().filter_map(|t| t.as_str()?.chars().next()).collect()).unwrap_or_default()
        };
        let enabled = |x: &serde_json::Value| x.is_object() || x.as_bool() == Some(true);
        ServerCaps {
            completion: c["completionProvider"].is_object(),
            completion_triggers: chars(&c["completionProvider"]["triggerCharacters"]),
            hover: enabled(&c["hoverProvider"]),
            signature_help: c["signatureHelpProvider"].is_object(),
            signature_triggers: chars(&c["signatureHelpProvider"]["triggerCharacters"]),
            rename: enabled(&c["renameProvider"]),
        }
    }
}

// ── LspManager ────────────────────────────────────────────────────────────────
pub struct LspManager {
    /// Keyed by output_pane_id (same as pane id for the LspOutput pane).
    pub servers: HashMap<usize, LspServer>,
}

impl LspManager {
    pub fn new() -> Self { LspManager { servers: HashMap::new() } }

    pub fn server_for_lang_host(&self, lang: Lang, ssh_host: Option<&SshHost>) -> Option<&LspServer> {
        self.servers.values().find(|s| s.lang == lang && s.ssh_host.as_ref() == ssh_host)
    }

    pub fn server_for_lang_host_mut(&mut self, lang: Lang, ssh_host: Option<&SshHost>) -> Option<&mut LspServer> {
        self.servers.values_mut().find(|s| s.lang == lang && s.ssh_host.as_ref() == ssh_host)
    }

    pub fn has_server_for_lang_host(&self, lang: Lang, ssh_host: Option<&SshHost>) -> bool {
        self.server_for_lang_host(lang, ssh_host).is_some()
    }
}

// ── JSON-RPC framing ──────────────────────────────────────────────────────────

/// Write a Content-Length-framed JSON-RPC message to the server's stdin.
fn write_message(stdin: &mut impl Write, body: &str) -> std::io::Result<()> {
    write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    stdin.flush()
}

/// Read one Content-Length-framed message from a buffered reader.
/// Returns None on EOF or parse error.
#[cfg(test)]
fn read_message<R: BufRead>(reader: &mut R) -> Option<String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 { return None; }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() { break; }
        if let Some(rest) = trimmed.strip_prefix("Content-Length: ") {
            content_length = rest.trim().parse().ok();
        }
    }
    let n = content_length?;
    if n > MAX_LSP_FRAME { return None; }
    let mut buf = vec![0u8; n];
    reader.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// Send a JSON-RPC notification (no id).
pub fn send_notification(server: &mut LspServer, method: &str, params: serde_json::Value) {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method":  method,
        "params":  params,
    }).to_string();
    server.writer.push(Outgoing::Raw(body));
}

/// Send a JSON-RPC request. Returns the request id.
pub fn send_request(server: &mut LspServer, method: &str, params: serde_json::Value) -> u64 {
    server.request_id += 1;
    let id = server.request_id;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id":      id,
        "method":  method,
        "params":  params,
    }).to_string();
    server.writer.push(Outgoing::Raw(body));
    id
}

// ── Diagnostic parsing ────────────────────────────────────────────────────────

/// If the message is a publishDiagnostics notification, return the (path,
/// diagnostics).  `ssh_host` is the remote host the server is running on (if
/// any); it is used to reconstruct the correct `VPath::Remote` from the
/// `file://` URI the server emits.
pub fn parse_diagnostics(json: &str, ssh_host: Option<&SshHost>) -> Option<(VPath, Vec<Diagnostic>)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if v["method"].as_str() != Some("textDocument/publishDiagnostics") { return None; }
    let params = &v["params"];
    let uri = params["uri"].as_str()?;
    let path = crate::vpath::lsp_uri_to_vpath(uri, ssh_host)?;
    let diags = params["diagnostics"].as_array()?.iter().filter_map(|d| {
        let s = &d["range"]["start"];
        let e = &d["range"]["end"];
        let severity = match d["severity"].as_u64().unwrap_or(1) {
            2 => DiagSeverity::Warning,
            3 => DiagSeverity::Info,
            4 => DiagSeverity::Hint,
            _ => DiagSeverity::Error,
        };
        Some(Diagnostic {
            line:      s["line"].as_u64()? as usize,
            col_start: s["character"].as_u64()? as usize,
            col_end:   e["character"].as_u64()? as usize,
            severity,
            message:   d["message"].as_str().unwrap_or("").to_owned(),
        })
    }).collect();
    Some((path, diags))
}

/// Check if the message is an `initialize` response so we know when to send `initialized`.
pub fn is_initialize_response(json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return false; };
    v["result"].is_object() && v["id"].as_u64() == Some(1)
}

// ── Server spawning ───────────────────────────────────────────────────────────

/// Built-in server command for a language.
fn default_command(lang: Lang) -> Option<(&'static str, &'static [&'static str])> {
    match lang {
        // rust-analyzer speaks LSP over stdin/stdout by default and *rejects* an
        // unknown `--stdio` flag ("unexpected flag: `--stdio`"), unlike the
        // Node-based servers below which require it.
        Lang::Rust       => Some(("rust-analyzer", &[])),
        Lang::TypeScript => Some(("typescript-language-server", &["--stdio"])),
        Lang::Python     => Some(("pylsp", &[])),
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown | Lang::Css | Lang::Html => None,
    }
}

/// The program and arguments that will actually run: per-project overrides
/// (`.vscode/local-text.json`) applied over the built-in defaults.
pub fn resolve_command(lang: Lang, overrides: &LspLaunchOverrides) -> Option<(String, Vec<String>)> {
    let (default_bin, default_args) = default_command(lang)?;
    let bin = overrides.command.clone().unwrap_or_else(|| default_bin.to_owned());
    let args = match &overrides.args {
        Some(a) => a.clone(),
        None    => default_args.iter().map(|s| (*s).to_owned()).collect(),
    };
    Some((bin, args))
}

/// Human-readable command line for the LSP output banner, showing overrides.
pub fn invocation_string(lang: Lang, overrides: &LspLaunchOverrides) -> Option<String> {
    let (bin, args) = resolve_command(lang, overrides)?;
    let mut parts: Vec<String> = overrides.env.iter().map(|(k, v)| format!("{k}={}", crate::ssh::shell_quote(v))).collect();
    parts.push(crate::ssh::shell_quote(&bin));
    parts.extend(args.iter().map(|a| crate::ssh::shell_quote(a)));
    Some(parts.join(" "))
}

/// Spawn a language server for the given language. Returns None if the binary
/// is not installed or the language has no configured server.
///
/// When `ssh_host` is `Some`, the server is launched on the remote host via
/// `ssh [user@]host -- <cmd> <args>`, reusing the ControlMaster socket.
pub fn start_server(
    lang: Lang,
    output_pane_id: usize,
    proxy: EventLoopProxy<UserEvent>,
    ssh_host: Option<SshHost>,
    remote_path_dirs: Vec<String>,
    cwd: Option<&VPath>,
    overrides: &LspLaunchOverrides,
) -> Option<LspServer> {
    let (bin, args) = resolve_command(lang, overrides)?;

    let mut child = if let Some(ref host) = ssh_host {
        let remote_cwd = cwd.and_then(|p| match p {
            VPath::Remote { host: cwd_host, path } if cwd_host == host => Some(path.as_path()),
            _ => None,
        });
        let mut ssh_cmd = ssh::ssh_lsp_command(host, &bin, &args, &remote_path_dirs, remote_cwd, &overrides.env);
        ssh_cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        ssh_cmd.spawn().ok()?
    } else {
        // Resolve through PATH ourselves: on Windows npm-installed servers are
        // `.cmd` shims, which `Command` (it only tries `.exe`) wouldn't find.
        let program = crate::os::find_executable(&bin).unwrap_or_else(|| bin.clone().into());
        let mut local_cmd = crate::os::command(&program);
        local_cmd.args(&args);
        for (k, v) in &overrides.env {
            local_cmd.env(k, v);
        }
        if let Some(VPath::Local(path)) = cwd {
            local_cmd.current_dir(path);
        }
        local_cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?
    };

    let stdin  = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // stdout reader: framed JSON-RPC → LspOutput (protocol only, not displayed);
    // unframed output (error tracebacks, Node.js crash messages, etc.) → TermOutput.
    let proxy_out = proxy.clone();
    let opi = output_pane_id;
    let host_for_thread = ssh_host.clone();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut first_line = String::new();
            match reader.read_line(&mut first_line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = first_line.trim_end_matches(['\r', '\n']);
            if let Some(rest) = trimmed.strip_prefix("Content-Length: ") {
                // Framed JSON-RPC message: parse for protocol, do not display
                let Ok(n) = rest.trim().parse::<usize>() else { continue };
                if n > MAX_LSP_FRAME {
                    // Never allocate an absurd frame; the stream is desynced/hostile.
                    let _ = proxy_out.send_event(UserEvent::TermOutput {
                        pane_id: opi,
                        data: format!(
                            "[lsp] Content-Length {n} exceeds {} MiB cap; closing reader\n",
                            MAX_LSP_FRAME >> 20,
                        ).into_bytes().into_boxed_slice(),
                    });
                    break;
                }
                // Consume remaining headers (up to the blank separator line)
                loop {
                    let mut hdr = String::new();
                    if reader.read_line(&mut hdr).unwrap_or(0) == 0 { break; }
                    if hdr.trim_end_matches(['\r', '\n']).is_empty() { break; }
                }
                let mut buf = vec![0u8; n];
                if reader.read_exact(&mut buf).is_err() { break; }
                let Ok(msg) = String::from_utf8(buf) else { continue };
                let _ = proxy_out.send_event(UserEvent::LspOutput {
                    pane_id: opi,
                    data:    msg.as_bytes().to_vec(),
                });
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) {
                    if let Some(line) = server_log_line(&v) {
                        let _ = proxy_out.send_event(UserEvent::TermOutput {
                            pane_id: opi,
                            data:    line.into_bytes().into_boxed_slice(),
                        });
                    }
                    if v["id"].is_u64() && v.get("method").is_none() {
                        let id = v["id"].as_u64().unwrap();
                        let result = v["result"].clone();
                        let error = v["error"]["message"].as_str().map(str::to_owned);
                        let _ = proxy_out.send_event(UserEvent::LspResponse { server_id: opi, id, result, error });
                    } else if let Some((path, diags)) = parse_diagnostics(&msg, host_for_thread.as_ref()) {
                        let _ = proxy_out.send_event(UserEvent::LspDiagnostics { path, diagnostics: diags });
                    }
                }
            } else {
                // Unframed stdout output (crash message, missing-module error, etc.): show in terminal
                let _ = proxy_out.send_event(UserEvent::TermOutput {
                    pane_id: opi,
                    data:    first_line.into_bytes().into_boxed_slice(),
                });
            }
        }
        let _ = proxy_out.send_event(UserEvent::LspServerStopped { server_id: opi });
    });

    // stderr reader: raw bytes → TermOutput (preserves ANSI colors from the server).
    // Read byte lines, not `lines()`: a non-UTF-8 line must not be dropped, and a
    // persistent read error must end the loop rather than spin.
    let proxy_err = proxy;
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if !line.ends_with(b"\n") { line.push(b'\n'); }
            let _ = proxy_err.send_event(UserEvent::TermOutput {
                pane_id: opi,
                data:    line.clone().into_boxed_slice(),
            });
        }
    });

    Some(LspServer {
        lang,
        process: child,
        writer: LspWriter::spawn(stdin),
        output_pane_id,
        request_id: 0,
        doc_version: HashMap::new(),
        initialized: false,
        pending: HashMap::new(),
        ssh_host,
        caps: ServerCaps::default(),
    })
}

// ── LSP notification helpers ──────────────────────────────────────────────────

/// Build textDocument/didOpen params.
pub fn did_open_params(path: &VPath, text: &str, lang_id: &str) -> serde_json::Value {
    serde_json::json!({
        "textDocument": {
            "uri":        path.to_lsp_uri(),
            "languageId": lang_id,
            "version":    1,
            "text":       text,
        }
    })
}

/// Build textDocument/didChange params (full document sync).
pub fn did_change_params(path: &VPath, text: &str, version: u64) -> serde_json::Value {
    serde_json::json!({
        "textDocument": {
            "uri":     path.to_lsp_uri(),
            "version": version,
        },
        "contentChanges": [{ "text": text }]
    })
}

/// Send LSP initialize + initialized handshake.
pub fn send_initialize(server: &mut LspServer, root_path: Option<&VPath>) {
    let root_uri = root_path.map(VPath::to_lsp_uri);
    let process_id = if server.ssh_host.is_some() { None } else { Some(std::process::id()) };
    let params = initialize_params(process_id, root_uri);
    send_request(server, "initialize", params);
}

fn initialize_params(process_id: Option<u32>, root_uri: Option<String>) -> serde_json::Value {
    serde_json::json!({
        "processId": process_id,
        "rootUri":   root_uri,
        "capabilities": {
            "textDocument": {
                "publishDiagnostics": { "relatedInformation": false },
                "synchronization":    { "didSave": true },
                "definition":         {},
                "references":         {},
                "formatting":         {},
                "completion": {
                    "completionItem": {
                        "snippetSupport": false,
                        "documentationFormat": ["plaintext", "markdown"],
                        "insertReplaceSupport": false
                    },
                    "contextSupport": true
                },
                "hover":         { "contentFormat": ["plaintext", "markdown"] },
                "signatureHelp": {
                    "signatureInformation": {
                        "documentationFormat": ["plaintext", "markdown"],
                        "parameterInformation": { "labelOffsetSupport": true }
                    },
                    "contextSupport": true
                },
                "rename": { "prepareSupport": false },
                "codeAction": {
                    "codeActionLiteralSupport": {
                        "codeActionKind": { "valueSet": ["source.organizeImports"] }
                    }
                }
            }
        }
    })
}

fn server_log_line(v: &serde_json::Value) -> Option<String> {
    match v["method"].as_str()? {
        "window/logMessage" | "window/showMessage" => {
            let message = v["params"]["message"].as_str()?.trim();
            if message.is_empty() { return None; }
            let level = match v["params"]["type"].as_u64().unwrap_or(3) {
                1 => "error",
                2 => "warning",
                4 => "log",
                _ => "info",
            };
            Some(format!("[lsp:{level}] {message}\n"))
        }
        "$/typescriptVersion" => {
            let version = v["params"]["version"].as_str()?;
            let source = v["params"]["source"].as_str().unwrap_or("unknown");
            Some(format!("[lsp:info] TypeScript {version} ({source})\n"))
        }
        _ => None,
    }
}

/// Send textDocument/didOpen. Before the initialize handshake has completed the
/// LSP spec forbids document notifications, so nothing is sent (and the document
/// isn't recorded as open); the caller re-opens every tab once `initialized` is sent.
pub fn notify_did_open(server: &mut LspServer, path: &VPath, text: &str) {
    if !server.initialized { return; }
    let lang_id = match server.lang {
        Lang::Rust       => "rust",
        Lang::TypeScript => "typescript",
        Lang::Python     => "python",
        Lang::Json       => "json",
        Lang::Jsonc      => "jsonc",
        Lang::Markdown   => "markdown",
        Lang::None | Lang::Css | Lang::Html => "text",
    };
    let params = did_open_params(path, text, lang_id);
    send_notification(server, "textDocument/didOpen", params);
    server.doc_version.insert(path.clone(), 1);
}

/// Send textDocument/didChange.
pub fn notify_did_change(server: &mut LspServer, path: &VPath, text: &str) {
    if !server.initialized { return; }
    let Some(ver) = server.doc_version.get_mut(path) else {
        // Never opened on this server — open it instead.
        notify_did_open(server, path, text);
        return;
    };
    *ver += 1;
    let version = *ver;
    let params = did_change_params(path, text, version);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method":  "textDocument/didChange",
        "params":  params,
    }).to_string();
    server.writer.push(Outgoing::DocChange { uri: path.to_lsp_uri(), body });
}

/// Send the `initialized` notification (after receiving initialize response).
pub fn send_initialized(server: &mut LspServer) {
    send_notification(server, "initialized", serde_json::json!({}));
    server.initialized = true;
}

/// Send textDocument/didClose and remove the tracked document version.
pub fn notify_did_close(server: &mut LspServer, path: &VPath) {
    if server.doc_version.remove(path).is_none() { return; } // never opened
    let params = serde_json::json!({
        "textDocument": { "uri": path.to_lsp_uri() }
    });
    send_notification(server, "textDocument/didClose", params);
}

pub fn request_definition(srv: &mut LspServer, path: &VPath, line: usize, col: usize) -> u64 {
    let id = send_request(srv, "textDocument/definition", serde_json::json!({
        "textDocument": { "uri": path.to_lsp_uri() },
        "position": { "line": line, "character": col }
    }));
    srv.pending.insert(id, PendingKind::Definition);
    id
}

pub fn request_references(srv: &mut LspServer, path: &VPath, line: usize, col: usize) -> u64 {
    let id = send_request(srv, "textDocument/references", serde_json::json!({
        "textDocument": { "uri": path.to_lsp_uri() },
        "position": { "line": line, "character": col },
        "context": { "includeDeclaration": false }
    }));
    srv.pending.insert(id, PendingKind::References);
    id
}

pub fn request_formatting(srv: &mut LspServer, path: &VPath, generation: u64, save_after: bool, indent: crate::layout::Indent) -> u64 {
    let id = send_request(srv, "textDocument/formatting", serde_json::json!({
        "textDocument": { "uri": path.to_lsp_uri() },
        "options": { "tabSize": indent.size, "insertSpaces": !indent.use_tabs }
    }));
    srv.pending.insert(id, PendingKind::Formatting { path: path.clone(), generation, save_after });
    id
}

pub fn request_organize_imports(srv: &mut LspServer, path: &VPath, generation: u64, then_format: bool, save_after: bool) -> u64 {
    let id = send_request(srv, "textDocument/codeAction", serde_json::json!({
        "textDocument": { "uri": path.to_lsp_uri() },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end":   { "line": 0, "character": 0 }
        },
        "context": { "only": ["source.organizeImports"], "diagnostics": [] }
    }));
    srv.pending.insert(id, PendingKind::OrganizeImports { path: path.clone(), generation, then_format, save_after });
    id
}

fn position_params(path: &VPath, line: usize, col: usize) -> serde_json::Value {
    serde_json::json!({
        "textDocument": { "uri": path.to_lsp_uri() },
        "position": { "line": line, "character": col }
    })
}

/// `trigger`: the trigger character typed, if that's what invoked it.
pub fn request_completion(srv: &mut LspServer, path: &VPath, line: usize, col: usize, anchor: usize, trigger: Option<char>) -> u64 {
    let mut params = position_params(path, line, col);
    params["context"] = match trigger {
        Some(ch) => serde_json::json!({ "triggerKind": 2, "triggerCharacter": ch.to_string() }),
        None => serde_json::json!({ "triggerKind": 1 }),
    };
    let id = send_request(srv, "textDocument/completion", params);
    srv.pending.insert(id, PendingKind::Completion { path: path.clone(), anchor });
    id
}

pub fn request_hover(srv: &mut LspServer, path: &VPath, line: usize, col: usize, pos: usize) -> u64 {
    let id = send_request(srv, "textDocument/hover", position_params(path, line, col));
    srv.pending.insert(id, PendingKind::Hover { path: path.clone(), pos });
    id
}

pub fn request_signature_help(srv: &mut LspServer, path: &VPath, line: usize, col: usize, trigger: Option<char>) -> u64 {
    let mut params = position_params(path, line, col);
    params["context"] = match trigger {
        Some(ch) => serde_json::json!({ "triggerKind": 2, "triggerCharacter": ch.to_string(), "isRetrigger": false }),
        None => serde_json::json!({ "triggerKind": 1, "isRetrigger": false }),
    };
    let id = send_request(srv, "textDocument/signatureHelp", params);
    srv.pending.insert(id, PendingKind::SignatureHelp { path: path.clone() });
    id
}

pub fn request_rename(srv: &mut LspServer, path: &VPath, line: usize, col: usize, new_name: &str, generation: u64) -> u64 {
    let mut params = position_params(path, line, col);
    params["newName"] = serde_json::Value::String(new_name.to_owned());
    let id = send_request(srv, "textDocument/rename", params);
    srv.pending.insert(id, PendingKind::Rename { path: path.clone(), generation });
    id
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn server_capabilities_are_read_from_initialize() {
        let msg = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{
            "completionProvider":{"triggerCharacters":[".",":"]},
            "hoverProvider":true,
            "signatureHelpProvider":{"triggerCharacters":["(",","]},
            "renameProvider":{"prepareProvider":true}}}}"#;
        let c = ServerCaps::from_initialize_response(msg);
        assert!(c.completion && c.hover && c.signature_help && c.rename);
        assert_eq!(c.completion_triggers, vec!['.', ':']);
        assert_eq!(c.signature_triggers, vec!['(', ',']);
        let none = ServerCaps::from_initialize_response(r#"{"id":1,"result":{"capabilities":{}}}"#);
        assert!(!none.completion && !none.hover && !none.rename);
    }

    #[test]
    fn read_message_parses_framed() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let raw = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut cur = Cursor::new(raw.as_bytes().to_vec());
        assert_eq!(read_message(&mut cur).as_deref(), Some(body));
    }

    #[test]
    fn read_message_rejects_oversized_content_length() {
        // A hostile/buggy server advertising a huge body must not trigger a giant
        // allocation — read_message returns None instead of allocating `n` bytes.
        let raw = format!("Content-Length: {}\r\n\r\n", MAX_LSP_FRAME + 1);
        let mut cur = Cursor::new(raw.into_bytes());
        assert_eq!(read_message(&mut cur), None);
    }

    #[test]
    fn read_message_returns_none_for_unframed() {
        // Simulates what happens when the server writes an error to stdout without framing
        let raw = "Error: Cannot find module 'typescript'\n";
        let mut cur = Cursor::new(raw.as_bytes().to_vec());
        // read_message sees the line as a header with no Content-Length, hits EOF → None
        assert_eq!(read_message(&mut cur), None);
    }

    #[test]
    fn stdout_reader_surfaces_unframed_lines() {
        // Simulate a stdout stream that starts with an unframed error then EOF.
        // The new stdout thread logic should send TermOutput for the unframed line.
        // We verify this by replaying the logic inline.
        let raw = b"Error: Cannot find module 'typescript'\n";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        let mut unframed_lines: Vec<String> = vec![];
        let mut framed_bodies: Vec<String> = vec![];
        loop {
            let mut first_line = String::new();
            match reader.read_line(&mut first_line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = first_line.trim_end_matches(['\r', '\n']);
            if let Some(rest) = trimmed.strip_prefix("Content-Length: ") {
                let Ok(n) = rest.trim().parse::<usize>() else { continue };
                loop {
                    let mut hdr = String::new();
                    if reader.read_line(&mut hdr).unwrap_or(0) == 0 { break; }
                    if hdr.trim_end_matches(['\r', '\n']).is_empty() { break; }
                }
                let mut buf = vec![0u8; n];
                if reader.read_exact(&mut buf).is_err() { break; }
                framed_bodies.push(String::from_utf8(buf).unwrap());
            } else {
                unframed_lines.push(first_line);
            }
        }
        assert_eq!(unframed_lines, vec!["Error: Cannot find module 'typescript'\n"]);
        assert!(framed_bodies.is_empty());
    }

    #[test]
    fn stdout_reader_handles_framed_then_unframed() {
        // Framed message followed by an unframed crash dump — both must be captured.
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let raw = format!(
            "Content-Length: {}\r\n\r\n{}node crashed\n",
            body.len(), body
        );
        let mut reader = BufReader::new(Cursor::new(raw.as_bytes().to_vec()));
        let mut unframed: Vec<String> = vec![];
        let mut framed: Vec<String> = vec![];
        loop {
            let mut first_line = String::new();
            match reader.read_line(&mut first_line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = first_line.trim_end_matches(['\r', '\n']);
            if let Some(rest) = trimmed.strip_prefix("Content-Length: ") {
                let Ok(n) = rest.trim().parse::<usize>() else { continue };
                loop {
                    let mut hdr = String::new();
                    if reader.read_line(&mut hdr).unwrap_or(0) == 0 { break; }
                    if hdr.trim_end_matches(['\r', '\n']).is_empty() { break; }
                }
                let mut buf = vec![0u8; n];
                if reader.read_exact(&mut buf).is_err() { break; }
                framed.push(String::from_utf8(buf).unwrap());
            } else {
                unframed.push(first_line);
            }
        }
        assert_eq!(framed, vec![body]);
        assert_eq!(unframed, vec!["node crashed\n"]);
    }

    #[test]
    fn queued_doc_changes_coalesce_but_keep_order_with_requests() {
        let mut q = OutQueue { items: VecDeque::new(), closed: false };
        let change = |uri: &str, body: &str| Outgoing::DocChange { uri: uri.into(), body: body.into() };
        assert!(enqueue(&mut q, change("a", "v1")));
        assert!(!enqueue(&mut q, change("a", "v2")));      // replaces v1
        assert!(enqueue(&mut q, change("b", "v1")));       // different doc
        assert!(enqueue(&mut q, Outgoing::Raw("req".into())));
        assert!(enqueue(&mut q, change("b", "v2")));       // after a request: appended
        let bodies: Vec<String> = q.items.iter().map(|o| match o {
            Outgoing::Raw(b) | Outgoing::DocChange { body: b, .. } => b.clone(),
        }).collect();
        assert_eq!(bodies, ["v2", "v1", "req", "v2"]);
        q.closed = true;
        assert!(!enqueue(&mut q, Outgoing::Raw("late".into())));
    }

    #[test]
    fn initialize_params_can_disable_process_monitoring() {
        let params = initialize_params(None, Some("file:///srv/app".to_owned()));
        assert!(params["processId"].is_null());
        assert_eq!(params["rootUri"].as_str(), Some("file:///srv/app"));

        let params = initialize_params(Some(123), None);
        assert_eq!(params["processId"].as_u64(), Some(123));
        assert!(params["rootUri"].is_null());
    }

    #[test]
    fn server_log_line_surfaces_lsp_messages() {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "window/logMessage",
            "params": { "type": 2, "message": "using fallback tsserver" }
        });
        assert_eq!(
            server_log_line(&msg).as_deref(),
            Some("[lsp:warning] using fallback tsserver\n"),
        );

        let version = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "$/typescriptVersion",
            "params": { "version": "6.0.3", "source": "bundled" }
        });
        assert_eq!(
            server_log_line(&version).as_deref(),
            Some("[lsp:info] TypeScript 6.0.3 (bundled)\n"),
        );
    }
}
