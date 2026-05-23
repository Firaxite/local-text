// SSH connection management — background file I/O and ControlMaster lifecycle.
//
// All public functions that touch the network run on background threads and
// communicate results back to the main loop via EventLoopProxy<UserEvent>.
//
// Transport: system `ssh` binary with ControlMaster socket multiplexing.
// This reuses the user's ~/.ssh/config, agent, jump hosts, and FIDO/Keychain
// auth without pulling in any SSH library (russh is AGPL; ssh2 bypasses config).
//
// Setup manifest: ~/.local/share/local-text/setup-manifest.json on each remote.
// Bumping REQUIRED_SETUP_VERSION triggers a re-run of the structural bootstrap
// script the next time that host is connected to.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use winit::event_loop::EventLoopProxy;

use crate::vpath::{SshHost, VPath};
use crate::UserEvent;

/// Increment this when the remote structural setup (directory layout, etc.)
/// needs to change.  LSP version changes do NOT require a bump.
pub const REQUIRED_SETUP_VERSION: u32 = 1;

// ── Public async entry points ─────────────────────────────────────────────────

/// Ensure the SSH ControlMaster socket for `host` exists (creating it if
/// needed).  Sends `SshConnected` on success or `SshError` on failure.
/// If the socket already exists, this returns immediately via `SshConnected`.
pub fn ensure_control_master(host: SshHost, proxy: EventLoopProxy<UserEvent>) {
    let socket = host.control_path();
    // Fast path: socket already exists — no new thread needed.
    if socket.exists() {
        let _ = proxy.send_event(UserEvent::SshConnected { host });
        return;
    }
    thread::spawn(move || {
        let _ = proxy.send_event(UserEvent::SshConnecting { host: host.clone() });
        match open_control_master(&host) {
            Ok(()) => {
                // Run setup check asynchronously after connecting.
                let host2 = host.clone();
                let proxy2 = proxy.clone();
                thread::spawn(move || check_setup(&host2, &proxy2));
                let _ = proxy.send_event(UserEvent::SshConnected { host });
            }
            Err(msg) => {
                let _ = proxy.send_event(UserEvent::SshError { host, msg });
            }
        }
    });
}

/// Read a file from a remote host.  Sends `RemoteFileContent` on success or
/// `SshError` on failure.
pub fn ssh_read_file(host: SshHost, path: PathBuf, token: u64, proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let vpath = VPath::Remote { host: host.clone(), path: path.clone() };
        match run_ssh_capture(&host, &["cat", &path.to_string_lossy()]) {
            Ok(content) => {
                let _ = proxy.send_event(UserEvent::RemoteFileContent { token, path: vpath, content });
            }
            Err(msg) => {
                let _ = proxy.send_event(UserEvent::SshError { host, msg });
            }
        }
    });
}

/// Write a file to a remote host.  Sends `RemoteWriteDone` on success or
/// `SshError` on failure.
pub fn ssh_write_file(host: SshHost, path: PathBuf, content: String, proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let vpath = VPath::Remote { host: host.clone(), path: path.clone() };
        match write_remote_file(&host, &path, &content) {
            Ok(()) => {
                let _ = proxy.send_event(UserEvent::RemoteWriteDone { path: vpath });
            }
            Err(msg) => {
                let _ = proxy.send_event(UserEvent::SshError { host, msg });
            }
        }
    });
}

/// List a remote directory.  Sends `RemoteDirListing` on success or `SshError`
/// on failure.  Each entry is `(name, is_dir)`.
pub fn ssh_list_dir(host: SshHost, path: PathBuf, proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let vpath = VPath::Remote { host: host.clone(), path: path.clone() };
        // Use `ls -1apL` for portable listing: trailing `/` marks directories,
        // `-a` includes hidden entries (we filter `.` and `..` below).
        let cmd = format!(
            "ls -1apL {} 2>/dev/null | head -2000",
            shell_quote(path.to_string_lossy().as_ref()),
        );
        match run_ssh_capture(&host, &["sh", "-c", &cmd]) {
            Ok(output) => {
                let entries: Vec<(String, bool)> = output.lines()
                    .filter(|l| !l.is_empty() && *l != "./" && *l != "../")
                    .map(|l| {
                        let is_dir = l.ends_with('/');
                        let name = l.trim_end_matches('/').to_owned();
                        (name, is_dir)
                    })
                    .collect();
                let _ = proxy.send_event(UserEvent::RemoteDirListing { path: vpath, entries });
            }
            Err(msg) => {
                let _ = proxy.send_event(UserEvent::SshError { host, msg });
            }
        }
    });
}

/// Walk all files under a remote directory (up to 50 000) for Cmd+P.
/// Sends `QuickFinderFiles` on completion.
pub fn ssh_walk_files(host: SshHost, root: PathBuf, token: u64, proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let cmd = format!(
            "find {} -type f -not -path '*/\\.*' 2>/dev/null | head -50000",
            shell_quote(root.to_string_lossy().as_ref()),
        );
        match run_ssh_capture(&host, &["sh", "-c", &cmd]) {
            Ok(output) => {
                let entries: Vec<VPath> = output.lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| VPath::Remote { host: host.clone(), path: PathBuf::from(l) })
                    .collect();
                let _ = proxy.send_event(UserEvent::QuickFinderFiles { token, entries });
            }
            Err(msg) => {
                let _ = proxy.send_event(UserEvent::SshError { host, msg });
            }
        }
    });
}

/// Build a `Command` that runs `cmd args` on the remote host, reusing the
/// ControlMaster socket.  Useful for spawning LSP servers via stdio.
pub fn ssh_lsp_command(host: &SshHost, bin: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o").arg(format!("ControlPath={}", host.control_path().display()))
       .arg("-o").arg("ControlMaster=no")  // reuse existing master only
       .arg(host.host_arg())
       .arg("--")
       .arg(bin)
       .args(args);
    cmd
}

// ── Setup versioning ──────────────────────────────────────────────────────────

/// Check the remote setup manifest and run bootstrap if needed.  Sends
/// `SshSetupNeeded` if the manifest is missing or outdated.
/// Also checks installed LSP versions and sends `LspVersionMismatch` if any
/// differ from what the manifest recorded.
fn check_setup(host: &SshHost, proxy: &EventLoopProxy<UserEvent>) {
    let manifest_path = "~/.local/share/local-text/setup-manifest.json";
    let cmd = format!("cat {manifest_path} 2>/dev/null || echo '__missing__'");
    let Ok(output) = run_ssh_capture(host, &["sh", "-c", &cmd]) else { return };

    let needs_bootstrap = if output.trim() == "__missing__" {
        true
    } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(&output) {
        v["setup_version"].as_u64().unwrap_or(0) < REQUIRED_SETUP_VERSION as u64
    } else {
        true
    };

    if needs_bootstrap {
        run_bootstrap(host, proxy);
    }
}

/// Run idempotent bootstrap on the remote: create the local-text data directory
/// and write the setup manifest.  This is purely structural — no LSP installs.
fn run_bootstrap(host: &SshHost, proxy: &EventLoopProxy<UserEvent>) {
    let manifest_json = serde_json::json!({
        "setup_version": REQUIRED_SETUP_VERSION,
        "lsp_versions":  {}
    }).to_string();
    // Escape for single-quoted shell string
    let json_escaped = manifest_json.replace('\'', "'\\''");
    let script = format!(
        "mkdir -p ~/.local/share/local-text && printf '%s' '{json_escaped}' > ~/.local/share/local-text/setup-manifest.json"
    );
    if let Err(msg) = run_ssh_capture(host, &["sh", "-c", &script]) {
        let _ = proxy.send_event(UserEvent::SshError { host: host.clone(), msg });
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Open (or reuse) the ControlMaster socket for `host`.
/// Blocks until the socket appears (up to 30 s) or the ssh process exits.
fn open_control_master(host: &SshHost) -> Result<(), String> {
    let socket = host.control_path();
    let mut child = Command::new("ssh")
        .arg("-o").arg("ControlMaster=auto")
        .arg("-o").arg(format!("ControlPath={}", socket.display()))
        .arg("-o").arg("ControlPersist=600")  // keep alive 10 min after last use
        .arg("-o").arg("BatchMode=yes")        // no interactive prompts
        .arg("-N")                             // don't execute a remote command
        .arg(host.host_arg())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ssh: {e}"))?;

    // Poll for the socket file to appear (ssh creates it once handshake done).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if socket.exists() { return Ok(()); }
        // Check if ssh exited (auth failure, host unreachable, etc.)
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr_out = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut stderr_out);
                }
                return Err(format!(
                    "ssh exited with {status}: {}", stderr_out.trim()
                ));
            }
            Ok(None) => {}  // still running
            Err(e) => return Err(format!("ssh wait error: {e}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err("Timed out waiting for SSH connection".to_owned());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Run a command on the remote and capture stdout as a String.
/// Uses the existing ControlMaster socket (does not create one).
fn run_ssh_capture(host: &SshHost, remote_argv: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o").arg(format!("ControlPath={}", host.control_path().display()))
       .arg("-o").arg("ControlMaster=no")
       .arg("-o").arg("BatchMode=yes")
       .arg(host.host_arg())
       .arg("--")
       .args(remote_argv)
       .stdin(Stdio::null())
       .stdout(Stdio::piped())
       .stderr(Stdio::piped());

    let output = cmd.output().map_err(|e| format!("ssh spawn error: {e}"))?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|_| "Remote output is not valid UTF-8".to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("ssh error ({}): {}", output.status, stderr.trim()))
    }
}

/// Write `content` to `remote_path` on `host` using `ssh … cat >`.
fn write_remote_file(host: &SshHost, remote_path: &Path, content: &str) -> Result<(), String> {
    let mut child = Command::new("ssh")
        .arg("-o").arg(format!("ControlPath={}", host.control_path().display()))
        .arg("-o").arg("ControlMaster=no")
        .arg("-o").arg("BatchMode=yes")
        .arg(host.host_arg())
        .arg("--")
        .arg("sh").arg("-c")
        .arg(format!("cat > {}", shell_quote(remote_path.to_string_lossy().as_ref())))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh spawn error: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content.as_bytes())
            .map_err(|e| format!("ssh write error: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("ssh wait error: {e}"))?;
    if status.success() { Ok(()) } else {
        Err(format!("ssh write failed with status {status}"))
    }
}

/// Single-quote a shell argument, escaping any embedded single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
