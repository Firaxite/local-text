// Language Server Protocol client.
//
// LspManager holds one LspServer per language.  Servers are started lazily when
// a file of the corresponding language is opened.  Each server gets its own
// OutputPane for its stdout/stderr log.
//
// JSON-RPC framing uses the LSP Content-Length header format.
// A background thread reads messages from the server's stdout and sends them
// to the main loop via EventLoopProxy<UserEvent>.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;

use winit::event_loop::EventLoopProxy;

use crate::ssh;
use crate::vpath::{SshHost, VPath};
use crate::{Diagnostic, DiagSeverity, Lang, UserEvent};

// ── PendingKind ───────────────────────────────────────────────────────────────
pub enum PendingKind {
    Definition,
    References,
    Formatting { path: VPath },
    OrganizeImports { path: VPath },
}

// ── LspServer ─────────────────────────────────────────────────────────────────
pub struct LspServer {
    pub lang:           Lang,
    pub process:        Child,
    pub stdin:          ChildStdin,
    pub output_pane_id: usize,
    pub request_id:     u64,
    pub doc_version:    HashMap<VPath, u64>,
    pub initialized:    bool,
    pub pending:        HashMap<u64, PendingKind>,
    /// If Some, this server runs on a remote host via SSH.
    pub ssh_host:       Option<SshHost>,
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
pub fn write_message(stdin: &mut ChildStdin, body: &str) -> std::io::Result<()> {
    write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    stdin.flush()
}

/// Read one Content-Length-framed message from a buffered reader.
/// Returns None on EOF or parse error.
pub fn read_message<R: BufRead>(reader: &mut R) -> Option<String> {
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
    let _ = write_message(&mut server.stdin, &body);
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
    let _ = write_message(&mut server.stdin, &body);
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
    let raw_path = PathBuf::from(uri.strip_prefix("file://")?);
    let path = match ssh_host {
        Some(host) => VPath::Remote { host: host.clone(), path: raw_path },
        None       => VPath::Local(raw_path),
    };
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
) -> Option<LspServer> {
    let (cmd, args): (&str, &[&str]) = match lang {
        Lang::Rust       => ("rust-analyzer", &["--stdio"]),
        Lang::TypeScript => ("typescript-language-server", &["--stdio"]),
        Lang::Python     => ("pylsp", &[]),
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown | Lang::Css | Lang::Html => return None,
    };

    let mut child = if let Some(ref host) = ssh_host {
        let mut ssh_cmd = ssh::ssh_lsp_command(host, cmd, args, &remote_path_dirs);
        ssh_cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        ssh_cmd.spawn().ok()?
    } else {
        Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?
    };

    let stdin  = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // stdout reader: JSON-RPC messages → LspOutput + LspDiagnostics/LspResponse events
    let proxy_out = proxy.clone();
    let opi = output_pane_id;
    let host_for_thread = ssh_host.clone();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Some(msg) = read_message(&mut reader) {
            // Log to output pane
            let _ = proxy_out.send_event(UserEvent::LspOutput {
                pane_id: opi,
                data:    msg.as_bytes().to_vec(),
            });
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) {
                if v["id"].is_u64() && v.get("method").is_none() {
                    // It's a response to a request
                    let id = v["id"].as_u64().unwrap();
                    let result = v["result"].clone();
                    let _ = proxy_out.send_event(UserEvent::LspResponse { server_id: opi, id, result });
                } else if let Some((path, diags)) = parse_diagnostics(&msg, host_for_thread.as_ref()) {
                    let _ = proxy_out.send_event(UserEvent::LspDiagnostics { path, diagnostics: diags });
                }
            }
        }
        let _ = proxy_out.send_event(UserEvent::LspServerStopped { server_id: opi });
    });

    // stderr reader: raw log lines → LspOutput events
    let proxy_err = proxy;
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().flatten() {
            let _ = proxy_err.send_event(UserEvent::LspOutput {
                pane_id: opi,
                data:    line.into_bytes(),
            });
        }
    });

    Some(LspServer {
        lang,
        process: child,
        stdin,
        output_pane_id,
        request_id: 0,
        doc_version: HashMap::new(),
        initialized: false,
        pending: HashMap::new(),
        ssh_host,
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
    let params = serde_json::json!({
        "processId": std::process::id(),
        "rootUri":   root_uri,
        "capabilities": {
            "textDocument": {
                "publishDiagnostics": { "relatedInformation": false },
                "synchronization":    { "didSave": true },
                "definition":         {},
                "references":         {},
                "formatting":         {},
                "codeAction": {
                    "codeActionLiteralSupport": {
                        "codeActionKind": { "valueSet": ["source.organizeImports"] }
                    }
                }
            }
        }
    });
    send_request(server, "initialize", params);
}

/// Send textDocument/didOpen.
pub fn notify_did_open(server: &mut LspServer, path: &VPath, text: &str) {
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
    let ver = server.doc_version.entry(path.clone()).or_insert(0);
    *ver += 1;
    let version = *ver;
    let params = did_change_params(path, text, version);
    send_notification(server, "textDocument/didChange", params);
}

/// Send the `initialized` notification (after receiving initialize response).
pub fn send_initialized(server: &mut LspServer) {
    send_notification(server, "initialized", serde_json::json!({}));
    server.initialized = true;
}

/// Send textDocument/didClose and remove the tracked document version.
pub fn notify_did_close(server: &mut LspServer, path: &VPath) {
    let params = serde_json::json!({
        "textDocument": { "uri": path.to_lsp_uri() }
    });
    send_notification(server, "textDocument/didClose", params);
    server.doc_version.remove(path);
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

pub fn request_formatting(srv: &mut LspServer, path: &VPath) -> u64 {
    let id = send_request(srv, "textDocument/formatting", serde_json::json!({
        "textDocument": { "uri": path.to_lsp_uri() },
        "options": { "tabSize": 4, "insertSpaces": true }
    }));
    srv.pending.insert(id, PendingKind::Formatting { path: path.clone() });
    id
}

pub fn request_organize_imports(srv: &mut LspServer, path: &VPath) -> u64 {
    let id = send_request(srv, "textDocument/codeAction", serde_json::json!({
        "textDocument": { "uri": path.to_lsp_uri() },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end":   { "line": 0, "character": 0 }
        },
        "context": { "only": ["source.organizeImports"], "diagnostics": [] }
    }));
    srv.pending.insert(id, PendingKind::OrganizeImports { path: path.clone() });
    id
}
