// OS services that differ between macOS, Linux and Windows: well-known
// directories, starting helper processes, finding executables, opening URLs,
// process liveness, canonical paths, and (Windows) command-line quoting and
// native dialogs. Rendering lives in `platform/`, the PTY in `terminal.rs`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── Directories ──────────────────────────────────────────────────────────────

/// The user's home directory: `$HOME` on Unix; `%USERPROFILE%` on Windows
/// (falling back to `$HOME`, which Git Bash / MSYS set).
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// Expand a leading `~` (`~`, `~/rest`, and on Windows `~\rest`) against the
/// home directory. None when `s` doesn't start that way or there is no home.
pub fn expand_home(s: &str) -> Option<PathBuf> {
    if s == "~" { return home_dir(); }
    let rest = s.strip_prefix("~/").or_else(|| if cfg!(windows) { s.strip_prefix("~\\") } else { None })?;
    Some(home_dir()?.join(rest))
}

/// An XDG base directory variable, honoured only when absolute (per the spec).
#[cfg(all(unix, not(target_os = "macos")))]
fn xdg_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute())
}

/// Where `config.json` lives:
///   macOS    ~/.config/local-text
///   Linux    $XDG_CONFIG_HOME/local-text (default ~/.config/local-text)
///   Windows  %APPDATA%\local-text
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    { std::env::var_os("APPDATA").filter(|d| !d.is_empty()).map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join("AppData").join("Roaming")))
        .map(|d| d.join("local-text")) }
    #[cfg(target_os = "macos")]
    { home_dir().map(|h| h.join(".config").join("local-text")) }
    #[cfg(all(unix, not(target_os = "macos")))]
    { xdg_dir("XDG_CONFIG_HOME").or_else(|| home_dir().map(|h| h.join(".config")))
        .map(|d| d.join("local-text")) }
}

/// Per-user application data (crash-recovery copies of unsaved buffers):
///   macOS    ~/.local/share/local-text
///   Linux    $XDG_DATA_HOME/local-text (default ~/.local/share/local-text)
///   Windows  %LOCALAPPDATA%\local-text
pub fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    { local_app_data().map(|d| d.join("local-text")) }
    #[cfg(target_os = "macos")]
    { home_dir().map(|h| h.join(".local").join("share").join("local-text")) }
    #[cfg(all(unix, not(target_os = "macos")))]
    { xdg_dir("XDG_DATA_HOME").or_else(|| home_dir().map(|h| h.join(".local").join("share")))
        .map(|d| d.join("local-text")) }
}

/// `%LOCALAPPDATA%` — per-user, per-machine, and not readable by other users
/// (the profile directory's ACL), so it also holds our runtime files.
#[cfg(windows)]
pub fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty()).map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join("AppData").join("Local")))
}

/// The login name, for the collab peer name.
pub fn user_name() -> Option<String> {
    ["USER", "USERNAME", "LOGNAME"].iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
}

// ── Processes ────────────────────────────────────────────────────────────────

/// A `Command` for a helper process (git, ssh, a language server, a formatter).
/// On Windows it starts without a console window: the editor is a GUI program,
/// so every console tool would otherwise flash up a console of its own.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    cmd
}

/// A user-supplied command line run through the platform shell (`sh -c` on
/// Unix, `cmd /d /s /c` on Windows), e.g. the external format command.
pub fn shell_command(command_line: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // `/s` makes cmd strip exactly the outer quotes and run the rest as typed;
        // passed raw so std's MSVC-style quoting (which cmd doesn't follow) isn't applied.
        let comspec = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut cmd = command(comspec);
        cmd.raw_arg(format!("/d /s /c \"{command_line}\""));
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = command("sh");
        cmd.arg("-c").arg(command_line);
        cmd
    }
}

/// Quote a local file path for `shell_command`.
pub fn shell_quote_path(path: &str) -> String {
    if cfg!(windows) {
        // Windows paths can't contain '"'; quoting keeps spaces, '&', '^' literal.
        format!("\"{path}\"")
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// Locate an executable the way a shell would: a name containing a path
/// separator is checked as given; a bare name is searched for in `PATH`. On
/// Windows every `PATHEXT` extension is tried (`.exe`, `.cmd`, …), so npm's
/// `typescript-language-server.cmd` shims are found — `Command::new` alone only
/// ever appends `.exe`.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    if name.is_empty() { return None; }
    let has_sep = name.contains('/') || (cfg!(windows) && name.contains('\\'));
    if has_sep {
        return executable_candidates(Path::new(name)).into_iter().find(|p| is_executable(p));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| executable_candidates(&dir.join(name)))
        .find(|p| is_executable(p))
}

/// `base` itself, plus on Windows `base` + each `PATHEXT` extension.
fn executable_candidates(base: &Path) -> Vec<PathBuf> {
    #[allow(unused_mut)]
    let mut out = vec![base.to_path_buf()];
    #[cfg(windows)]
    {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
        let has_known_ext = base.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            exts.split(';').any(|x| x.trim_start_matches('.').eq_ignore_ascii_case(e))
        });
        if !has_known_ext { out.clear(); } // "foo" alone is never runnable; "foo.exe" is
        for ext in exts.split(';').filter(|e| !e.is_empty()) {
            let mut s = base.as_os_str().to_owned();
            s.push(ext.to_ascii_lowercase());
            out.push(PathBuf::from(s));
        }
    }
    out
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    { p.is_file() }
}

/// Open a URL in the default browser (macOS `open`, Linux `xdg-open`, Windows
/// ShellExecute).
pub fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |s: &OsStr| s.encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (verb, target) = (wide(OsStr::new("open")), wide(OsStr::new(url)));
        // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call.
        let r = unsafe {
            ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), target.as_ptr(),
                std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
        };
        // ShellExecute reports success as a value > 32.
        if r as usize > 32 { Ok(()) } else { Err(std::io::Error::other("ShellExecute failed")) }
    }
    #[cfg(not(windows))]
    {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let mut child = command(opener).arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        std::thread::spawn(move || { let _ = child.wait(); }); // reap: no zombie
        Ok(())
    }
}

/// True if a process with this id exists. (Callers guard against pid reuse
/// separately.)
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    { unsafe { libc::kill(pid as libc::pid_t, 0) == 0 } }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() { return false; }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }
}

// ── Paths ────────────────────────────────────────────────────────────────────

/// `std::fs::canonicalize`, but on Windows without the `\\?\` prefix it adds
/// whenever that can be dropped safely: `\\?\C:\x` would otherwise show up in
/// titles, become `file:////?/C:/x` for language servers, and never compare equal
/// to the same path typed or listed normally.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(simplify_verbatim)
}

fn simplify_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let Some(s) = p.to_str() else { return p };
        // Longer paths only work in verbatim form.
        const MAX_PATH: usize = 260;
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            if rest.len() + 2 < MAX_PATH { return PathBuf::from(format!(r"\\{rest}")); }
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            let b = rest.as_bytes();
            if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' && rest.len() < MAX_PATH {
                return PathBuf::from(rest);
            }
        }
    }
    p
}

// ── Windows command lines ────────────────────────────────────────────────────

/// Quote one argument for a Windows command line so the MSVC runtime's parser
/// (`CommandLineToArgvW` rules) hands it back to the program unchanged.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn windows_quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are doubled, plus one to escape it.
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Trailing backslashes are doubled so they don't escape the closing quote.
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// A whole Windows command line from an argv.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn windows_command_line<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter().map(|a| windows_quote_arg(a.as_ref())).collect::<Vec<_>>().join(" ")
}

// ── Windows dialogs ──────────────────────────────────────────────────────────

/// Native dialogs on Windows. They're modal to the editor window via
/// `MB_TASKMODAL`/no owner, and like the macOS AppleScript dialogs they block the
/// event loop while open.
#[cfg(windows)]
pub mod dialogs {
    use std::ffi::OsStr;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
        OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDNO, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_TASKMODAL, MB_YESNOCANCEL,
    };

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// A file open/save dialog; None if cancelled.
    fn file_dialog(save: bool, title: &str, start_dir: Option<&str>) -> Option<PathBuf> {
        // Room for long paths; the dialog fails (returns 0) rather than truncating.
        let mut buf = vec![0u16; 32 * 1024];
        let filter: Vec<u16> = "All files (*.*)\0*.*\0\0".encode_utf16().collect();
        let title_w = wide(title);
        let dir_w = start_dir.map(wide);
        // SAFETY: OPENFILENAMEW is plain data; zeroed is its documented default.
        let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.nFilterIndex = 1;
        ofn.lpstrFile = buf.as_mut_ptr();
        ofn.nMaxFile = buf.len() as u32;
        ofn.lpstrTitle = title_w.as_ptr();
        ofn.lpstrInitialDir = dir_w.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
        ofn.Flags = OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST
            | if save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST };
        // SAFETY: every pointer in `ofn` refers to a live buffer above.
        let ok = unsafe { if save { GetSaveFileNameW(&mut ofn) } else { GetOpenFileNameW(&mut ofn) } };
        if ok == 0 { return None; }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (len > 0).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..len])))
    }

    pub fn open_file(start_dir: Option<&str>) -> Option<PathBuf> {
        file_dialog(false, "Open File", start_dir)
    }

    pub fn save_file(start_dir: Option<&str>) -> Option<PathBuf> {
        file_dialog(true, "Save As", start_dir)
    }

    /// Yes / No / Cancel question: Some(true) = Yes, Some(false) = No, None = Cancel.
    pub fn yes_no_cancel(message: &str, title: &str) -> Option<bool> {
        let (text, caption) = (wide(message), wide(title));
        // SAFETY: NUL-terminated UTF-16 strings that outlive the call.
        let r = unsafe {
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(),
                MB_YESNOCANCEL | MB_ICONWARNING | MB_TASKMODAL | MB_SETFOREGROUND)
        };
        match r {
            IDYES => Some(true),
            IDNO => Some(false),
            _ => None,
        }
    }

    /// OK / Cancel question: true = OK.
    pub fn ok_cancel(message: &str, title: &str) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{IDOK, MB_OKCANCEL};
        let (text, caption) = (wide(message), wide(title));
        // SAFETY: NUL-terminated UTF-16 strings that outlive the call.
        let r = unsafe {
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(),
                MB_OKCANCEL | MB_ICONWARNING | MB_TASKMODAL | MB_SETFOREGROUND)
        };
        r == IDOK
    }

    /// Single-line text prompt. Win32 has no stock input box, so this uses the
    /// .NET one through Windows PowerShell (part of every Windows install). The
    /// texts travel in environment variables, never through the command line, so
    /// nothing in them is interpreted. None if cancelled.
    pub fn prompt(message: &str, default: &str) -> Option<String> {
        const SCRIPT: &str = "Add-Type -AssemblyName Microsoft.VisualBasic; \
            [Console]::OutputEncoding = [Text.Encoding]::UTF8; \
            $r = [Microsoft.VisualBasic.Interaction]::InputBox($env:LT_PROMPT_TEXT, 'local-text', $env:LT_PROMPT_DEFAULT); \
            if ($r -eq '') { exit 1 }; [Console]::Out.Write($r)";
        let out = super::command("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", SCRIPT])
            .env("LT_PROMPT_TEXT", message)
            .env("LT_PROMPT_DEFAULT", default)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output().ok()?;
        if !out.status.success() { return None; }
        let text = String::from_utf8(out.stdout).ok()?;
        let text = text.trim_end_matches(['\r', '\n']);
        (!text.is_empty()).then(|| text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_quoting_round_trips_msvc_rules() {
        assert_eq!(windows_quote_arg("plain"), "plain");
        assert_eq!(windows_quote_arg(""), "\"\"");
        assert_eq!(windows_quote_arg("a b"), "\"a b\"");
        assert_eq!(windows_quote_arg(r"C:\Program Files\x\"), r#""C:\Program Files\x\\""#);
        assert_eq!(windows_quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(windows_quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(windows_quote_arg(r"C:\dir\file"), r"C:\dir\file"); // no quoting needed
        assert_eq!(
            windows_command_line(&["ssh", "-t", "host", "cd '/a b' && exec \"$SHELL\" -l"]),
            r#"ssh -t host "cd '/a b' && exec \"$SHELL\" -l""#,
        );
    }

    #[test]
    fn finds_executables_on_path() {
        #[cfg(unix)]
        {
            assert!(find_executable("sh").is_some());
            assert_eq!(find_executable("/bin/sh").as_deref(), Some(Path::new("/bin/sh")));
        }
        assert!(find_executable("no-such-program-local-text").is_none());
        assert!(find_executable("").is_none());
    }

    #[test]
    fn expands_home_prefix_only() {
        if let Some(home) = home_dir() {
            assert_eq!(expand_home("~"), Some(home.clone()));
            assert_eq!(expand_home("~/a/b"), Some(home.join("a/b")));
        }
        assert_eq!(expand_home("~user/x"), None);
        assert_eq!(expand_home("/abs"), None);
    }

    #[test]
    fn own_process_is_alive() {
        assert!(pid_alive(std::process::id()));
    }
}
