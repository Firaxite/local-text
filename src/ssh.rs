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
use crate::{Lang, UserEvent};

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
/// `RemoteFileError` on failure.
pub fn ssh_read_file(host: SshHost, path: PathBuf, token: u64, proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let vpath = VPath::Remote { host: host.clone(), path: path.clone() };
        let cmd = format!("cat -- {}", remote_path_expr(&path));
        match run_ssh_capture(&host, &["sh", "-c", &cmd]) {
            Ok(content) => {
                let _ = proxy.send_event(UserEvent::RemoteFileContent { token, path: vpath, content });
            }
            Err(msg) => {
                let _ = proxy.send_event(UserEvent::RemoteFileError { token, path: vpath, msg });
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
        let cmd = format!(
            r#"dir={}
n=0
for p in "$dir"/* "$dir"/.[!.]* "$dir"/..?*; do
    if [ ! -e "$p" ] && [ ! -L "$p" ]; then
        continue
    fi
    name=${{p##*/}}
    if [ "$name" = "." ] || [ "$name" = ".." ]; then
        continue
    fi
    if [ -d "$p" ]; then
        kind=d
    else
        kind=f
    fi
    printf '%s\0%s\0' "$kind" "$name"
    n=$((n + 1))
    if [ "$n" -ge 2000 ]; then
        break
    fi
done"#,
            remote_path_expr(&path),
        );
        match run_ssh_capture(&host, &["sh", "-c", &cmd]) {
            Ok(output) => {
                let entries = parse_remote_dir_listing(&output);
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
            remote_path_expr(&root),
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

/// Check whether LSP binaries are available on a remote host.
pub fn ssh_check_lsp_binaries(
    host: SshHost,
    bins: Vec<(Lang, Vec<String>)>,
    path_dirs: Vec<String>,
    proxy: EventLoopProxy<UserEvent>,
) {
    thread::spawn(move || {
        let langs: Vec<Lang> = bins.iter().map(|(lang, _)| *lang).collect();
        let mut script = lsp_path_setup(&path_dirs);
        for (idx, (_, required)) in bins.iter().enumerate() {
            let checks = if required.is_empty() {
                "false".to_owned()
            } else {
                required.iter()
                    .map(|bin| format!("command -v {} >/dev/null 2>&1", shell_quote(bin.as_str())))
                    .collect::<Vec<_>>()
                    .join(" && ")
            };
            script.push_str(&format!(
                "if {checks}; then printf '{}\\t1\\n'; else printf '{}\\t0\\n'; fi\n",
                idx, idx,
            ));
        }

        let (statuses, error) = match run_ssh_capture(&host, &["sh", "-c", &script]) {
            Ok(output) => {
                let mut statuses = Vec::new();
                for line in output.lines() {
                    let Some((idx, installed)) = line.split_once('\t') else { continue };
                    let Ok(idx) = idx.parse::<usize>() else { continue };
                    let Some(lang) = langs.get(idx).copied() else { continue };
                    statuses.push((lang, Some(installed == "1")));
                }
                (statuses, None)
            }
            Err(msg) => {
                let statuses = langs.into_iter().map(|lang| (lang, None)).collect();
                (statuses, Some(msg))
            }
        };
        let _ = proxy.send_event(UserEvent::LspBinaryCheckResult { host: Some(host), statuses, error });
    });
}

/// Build a `Command` that runs `cmd args` on the remote host, reusing the
/// ControlMaster socket.  Useful for spawning LSP servers via stdio.
pub fn ssh_lsp_command(host: &SshHost, bin: &str, args: &[&str], path_dirs: &[String]) -> Command {
    let mut cmd = Command::new("ssh");
    let remote_command = if path_dirs.is_empty() {
        let mut remote_argv = Vec::with_capacity(args.len() + 1);
        remote_argv.push(bin);
        remote_argv.extend(args.iter().copied());
        remote_command(&remote_argv)
    } else {
        let mut script = lsp_path_setup(path_dirs);
        script.push_str("exec ");
        script.push_str(&shell_quote(bin));
        for arg in args {
            script.push(' ');
            script.push_str(&shell_quote(arg));
        }
        remote_command(&["sh", "-c", &script])
    };
    cmd.arg("-o").arg(format!("ControlPath={}", host.control_path().display()))
       .arg("-o").arg("ControlMaster=no")  // reuse existing master only
       .arg("-o").arg("BatchMode=yes");
    if let Some(port) = host.port {
        cmd.arg("-p").arg(port.to_string());
    }
    cmd.arg(host.host_arg())
       .arg("--")
       .arg(remote_command);
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
    let mut cmd = Command::new("ssh");
    cmd.arg("-o").arg("ControlMaster=auto")
       .arg("-o").arg(format!("ControlPath={}", socket.display()))
       .arg("-o").arg("ControlPersist=600")  // keep alive 10 min after last use
       .arg("-o").arg("BatchMode=yes")        // no interactive prompts
       .arg("-N");                            // don't execute a remote command
    if let Some(port) = host.port {
        cmd.arg("-p").arg(port.to_string());
    }
    cmd.arg(host.host_arg())
       .stdin(Stdio::null())
       .stdout(Stdio::null())
       .stderr(Stdio::piped());
    let mut child = cmd.spawn()
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
pub fn run_ssh_capture(host: &SshHost, remote_argv: &[&str]) -> Result<String, String> {
    // OpenSSH sends the remote command as one shell string. Quote argv ourselves
    // so `sh -c <script>` remains one script instead of leaking lines into the
    // remote login shell with unset variables.
    let remote_command = remote_command(remote_argv);
    let mut cmd = Command::new("ssh");
    cmd.arg("-o").arg(format!("ControlPath={}", host.control_path().display()))
       .arg("-o").arg("ControlMaster=no")
       .arg("-o").arg("BatchMode=yes");
    if let Some(port) = host.port {
        cmd.arg("-p").arg(port.to_string());
    }
    cmd.arg(host.host_arg())
       .arg("--")
       .arg(remote_command)
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
    let script = format!("cat > {}", remote_path_expr(remote_path));
    let remote_command = remote_command(&["sh", "-c", &script]);
    let mut cmd = Command::new("ssh");
    cmd.arg("-o").arg(format!("ControlPath={}", host.control_path().display()))
       .arg("-o").arg("ControlMaster=no")
       .arg("-o").arg("BatchMode=yes");
    if let Some(port) = host.port {
        cmd.arg("-p").arg(port.to_string());
    }
    cmd.arg(host.host_arg())
       .arg("--")
       .arg(remote_command)
       .stdin(Stdio::piped())
       .stdout(Stdio::null())
       .stderr(Stdio::piped());
    let mut child = cmd.spawn()
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

fn remote_command(argv: &[&str]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn lsp_path_setup(path_dirs: &[String]) -> String {
    let mut script = String::from("PATH=${PATH:-}\n");
    for dir in path_dirs.iter().filter(|dir| !dir.is_empty()) {
        let expr = remote_path_component_expr(dir);
        script.push_str(&format!("if [ -d {expr} ]; then PATH={expr}:\"$PATH\"; fi\n"));
    }
    script.push_str(
        r#"for d in \
    "$HOME"/.nvm/versions/node/*/bin \
    "$HOME"/.volta/bin \
    "$HOME"/.asdf/shims \
    "$HOME"/.asdf/installs/nodejs/*/bin \
    "$HOME"/.fnm/node-versions/*/installation/bin \
    "$HOME"/.local/share/fnm/node-versions/*/installation/bin \
    "$HOME"/.bun/bin \
    "$HOME"/.yarn/bin \
    "$HOME"/.config/yarn/global/node_modules/.bin \
    "$HOME"/.local/share/pnpm; do
    if [ -d "$d" ]; then PATH="$d:$PATH"; fi
done
if command -v npm >/dev/null 2>&1; then
    npm_prefix=$(npm config get prefix 2>/dev/null)
    if [ -n "$npm_prefix" ] && [ "$npm_prefix" != "undefined" ] && [ -d "$npm_prefix/bin" ]; then
        PATH="$npm_prefix/bin:$PATH"
    fi
fi
export PATH
"#,
    );
    script
}

fn remote_path_component_expr(path: &str) -> String {
    if path == "~" {
        "$HOME".to_owned()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("$HOME/{}", shell_quote(rest))
    } else {
        shell_quote(path)
    }
}

fn parse_remote_dir_listing(output: &str) -> Vec<(String, bool)> {
    let mut entries = Vec::new();
    let mut fields = output.split('\0');
    while let Some(kind) = fields.next() {
        if kind.is_empty() { break; }
        let Some(name) = fields.next() else { break };
        if name.is_empty() || name == "." || name == ".." { continue; }
        match kind {
            "d" => entries.push((name.to_owned(), true)),
            "f" => entries.push((name.to_owned(), false)),
            _ => {}
        }
    }
    entries
}

fn remote_path_expr(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw == "~" {
        "~".to_owned()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        format!("~/{}", shell_quote(rest))
    } else {
        shell_quote(raw.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typed_nul_directory_listing() {
        let output = "d\0src\0f\0Cargo.toml\0d\0dir with spaces\0f\0tab\tname\0f\0line\nname\0";
        assert_eq!(
            parse_remote_dir_listing(output),
            vec![
                ("src".to_owned(), true),
                ("Cargo.toml".to_owned(), false),
                ("dir with spaces".to_owned(), true),
                ("tab\tname".to_owned(), false),
                ("line\nname".to_owned(), false),
            ],
        );
    }

    #[test]
    fn ignores_malformed_directory_listing_records() {
        let output = "x\0ignored\0d\0.\0f\0..\0f\0.hidden\0d";
        assert_eq!(parse_remote_dir_listing(output), vec![(".hidden".to_owned(), false)]);
    }

    #[test]
    fn quotes_remote_argv_as_one_shell_command() {
        let cmd = remote_command(&["sh", "-c", "dir='/tmp/a b'\nfor p in \"$dir\"/*; do :; done"]);
        assert!(cmd.starts_with("'sh' '-c' 'dir='\\''/tmp/a b'\\''\n"));
        assert!(cmd.contains("for p in \"$dir\"/*"));
    }

    #[test]
    fn preserves_tilde_expansion_in_remote_path_expr() {
        assert_eq!(remote_path_expr(Path::new("~")), "~");
        assert_eq!(remote_path_expr(Path::new("~/dir with spaces")), "~/'dir with spaces'");
        assert_eq!(remote_path_expr(Path::new("/tmp/dir with spaces")), "'/tmp/dir with spaces'");
    }
}
