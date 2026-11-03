// Virtual path — abstracts over local filesystem paths and remote SSH paths.
//
// VPath::Local  wraps a std::path::PathBuf and behaves identically to the
//               current local-only code.
// VPath::Remote carries an SshHost plus the absolute path on that host.
//
// All methods are designed so that callers which only care about the *shape*
// of the path (extension, file_name, display) do not need to branch on
// local-vs-remote.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

// ── SshHost ───────────────────────────────────────────────────────────────────

/// Identity of an SSH remote host, including optional user and port.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SshHost {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// A per-user directory for SSH ControlMaster sockets, created 0700 on demand. Prefers
/// `$XDG_RUNTIME_DIR` (already private on Linux); otherwise a uid-scoped directory under
/// the system temp dir (macOS's `$TMPDIR` is itself per-user and private). Kept short to
/// respect the ~104-byte `sun_path` limit for Unix-domain sockets: macOS's `$TMPDIR`
/// (`/var/folders/…/T/`) is ~49 bytes and can be longer still under an app sandbox, so if a
/// preferred location would leave too little room for the socket filename we fall back to a
/// short `/tmp/lt-<uid>` directory (still created 0700 and ownership-checked).
pub(crate) fn control_socket_dir() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    // The socket filename is a fixed "ssh-" + 16 hex digits = 21 bytes (incl. separator),
    // so cap the directory length to keep the full path well under the sun_path limit.
    const MAX_DIR_LEN: usize = 80;

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(rt).join("local-text"));
    }
    candidates.push(std::env::temp_dir().join(format!("local-text-{uid}")));

    for dir in &candidates {
        if dir.as_os_str().len() <= MAX_DIR_LEN && ensure_private_dir(dir).is_ok() {
            return dir.clone();
        }
    }

    // Guaranteed-short fallback.
    let dir = PathBuf::from(format!("/tmp/lt-{uid}"));
    let _ = ensure_private_dir(&dir);
    dir
}

/// FNV-1a 64-bit hash — deterministic across runs (so a reconnect reuses the same
/// ControlMaster socket) and dependency-free. Used only to derive a short, fixed-length
/// socket filename from the (user, host, port) tag; not for security.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Ensure `dir` exists as a 0700 directory owned by the current user. Refuses (Err) if it
/// already exists as a symlink or is owned by someone else, so a pre-created path can't
/// redirect our sockets. Best-effort hardening against a local same-host attacker.
pub(crate) fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::symlink_metadata(dir) {
        Ok(md) => {
            let uid = unsafe { libc::getuid() };
            if !md.is_dir() || md.uid() != uid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "control socket dir not owned by current user",
                ));
            }
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Err(_) => {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
    }
    Ok(())
}

impl SshHost {
    /// The ControlMaster socket path used to multiplex connections.
    ///
    /// Lives in a per-user 0700 directory (see `control_socket_dir`) rather than directly
    /// in world-writable `/tmp` under a predictable name, so another local user can't squat
    /// it. The directory is created on demand. The filename is a short hash of the
    /// `user-host-port` tag rather than the raw tag, so an arbitrarily long hostname can't
    /// push the socket path past the ~104-byte `sun_path` limit.
    pub fn control_path(&self) -> PathBuf {
        let tag = format!(
            "{}-{}-{}",
            self.user.as_deref().unwrap_or(""),
            self.host,
            self.port.unwrap_or(22),
        );
        control_socket_dir().join(format!("ssh-{:016x}", fnv1a(tag.as_bytes())))
    }

    /// The `[user@]host` argument passed to the `ssh` command.
    pub fn host_arg(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None    => self.host.clone(),
        }
    }

    /// Human-readable display: `user@host`, `host:port`, etc.
    pub fn display(&self) -> String {
        match (&self.user, self.port) {
            (Some(u), Some(p)) => format!("{u}@{}:{p}", self.host),
            (Some(u), None)    => format!("{u}@{}", self.host),
            (None,    Some(p)) => format!("{}:{p}", self.host),
            (None,    None)    => self.host.clone(),
        }
    }
}

impl PartialOrd for SshHost {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for SshHost {
    fn cmp(&self, other: &Self) -> Ordering {
        self.host.cmp(&other.host)
            .then_with(|| self.user.cmp(&other.user))
            .then_with(|| self.port.cmp(&other.port))
    }
}

// ── VPath ─────────────────────────────────────────────────────────────────────

/// A path that may refer to a local file or a file on a remote SSH host.
///
/// The inner `PathBuf` for `Remote` is always an absolute POSIX path on the
/// remote machine — it is never a local filesystem path.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum VPath {
    Local(PathBuf),
    Remote { host: SshHost, path: PathBuf },
}

impl VPath {
    // ── Construction ─────────────────────────────────────────────────────────

    /// Parse `"ssh://[user@]host[:port]:path"` → `VPath::Remote`.
    /// If `:path` is omitted, the path defaults to `~` on the remote.
    /// or any other string → `VPath::Local`.
    ///
    /// A leading `~` is kept as-is: the remote home directory isn't known until we
    /// connect (it is `/root` for root, `/Users/<name>` on macOS, …). Remote shell
    /// commands expand it themselves, and `resolve_remote_tilde` rewrites it to the
    /// real absolute path once `$HOME` has been read from the host.
    pub fn parse(s: &str) -> Self {
        if let Some(rest) = s.strip_prefix("ssh://") {
            let Some((host_part, path_part)) = split_ssh_host_and_path(rest) else {
                return VPath::Local(PathBuf::from(s));
            };
            if host_part.is_empty() || host_part.contains('/') {
                return VPath::Local(PathBuf::from(s));
            }

            let (user, host_port) = if let Some(at) = host_part.rfind('@') {
                (Some(host_part[..at].to_owned()), &host_part[at + 1..])
            } else {
                (None, host_part)
            };

            let (host, port) = if let Some(colon) = host_port.rfind(':') {
                let maybe_port = &host_port[colon + 1..];
                if let Ok(p) = maybe_port.parse::<u16>() {
                    (host_port[..colon].to_owned(), Some(p))
                } else {
                    (host_port.to_owned(), None)
                }
            } else {
                (host_port.to_owned(), None)
            };

            if !host.is_empty() {
                let ssh_host = SshHost { user, host, port };
                let path = PathBuf::from(path_part);
                return VPath::Remote { host: ssh_host, path };
            }
        }
        VPath::Local(PathBuf::from(s))
    }

    // ── Path component accessors ──────────────────────────────────────────────

    /// The last path component (file name), for use in tab titles.
    pub fn file_name(&self) -> Option<&OsStr> {
        match self {
            VPath::Local(p)            => p.file_name(),
            VPath::Remote { path, .. } => path.file_name(),
        }
    }

    /// The file extension, used for language detection.
    pub fn extension(&self) -> Option<&OsStr> {
        match self {
            VPath::Local(p)            => p.extension(),
            VPath::Remote { path, .. } => path.extension(),
        }
    }

    /// The parent directory as a new VPath, or None if already at root.
    pub fn parent(&self) -> Option<VPath> {
        match self {
            VPath::Local(p) => p.parent().map(|p| VPath::Local(p.to_path_buf())),
            VPath::Remote { host, path } => path.parent().map(|p| VPath::Remote {
                host: host.clone(),
                path: p.to_path_buf(),
            }),
        }
    }

    /// Append a path component.
    pub fn join(&self, component: impl AsRef<Path>) -> VPath {
        match self {
            VPath::Local(p) => VPath::Local(p.join(component)),
            VPath::Remote { host, path } => VPath::Remote {
                host: host.clone(),
                path: path.join(component),
            },
        }
    }

    /// Strip a prefix, returning the suffix as a plain `&Path`.
    /// Returns `None` if the prefix doesn't match or the two paths are on
    /// different hosts.
    pub fn strip_prefix<'a>(&'a self, base: &VPath) -> Option<&'a Path> {
        match (self, base) {
            (VPath::Local(p), VPath::Local(b)) => p.strip_prefix(b).ok(),
            (VPath::Remote { host: h1, path: p }, VPath::Remote { host: h2, path: b })
                if h1 == h2 => p.strip_prefix(b).ok(),
            _ => None,
        }
    }

    // ── Classification ────────────────────────────────────────────────────────

    /// True if this path refers to a remote host.
    pub fn is_remote(&self) -> bool { matches!(self, VPath::Remote { .. }) }

    /// The SSH host if this is a remote path.
    pub fn ssh_host(&self) -> Option<&SshHost> {
        match self {
            VPath::Remote { host, .. } => Some(host),
            _ => None,
        }
    }

    // ── Path borrowing ────────────────────────────────────────────────────────

    /// Borrow the inner path component (local or remote) as a `&Path`.
    /// Useful for extension-based language detection, where the host doesn't
    /// matter.
    pub fn as_path(&self) -> &Path {
        match self {
            VPath::Local(p)            => p.as_path(),
            VPath::Remote { path, .. } => path.as_path(),
        }
    }

    /// Borrow the local `Path` if this is a local path; `None` for remote.
    pub fn as_local_path(&self) -> Option<&Path> {
        match self {
            VPath::Local(p) => Some(p.as_path()),
            _ => None,
        }
    }

    // ── LSP / display ─────────────────────────────────────────────────────────

    /// Produce the `file://` URI sent to the LSP server.
    ///
    /// For remote paths we still produce `file:///remote/path` because the
    /// LSP server process runs *on the remote*, where that path is local.
    pub fn to_lsp_uri(&self) -> String {
        match self {
            VPath::Local(p) => path_to_file_uri(p),
            VPath::Remote { host, path } => path_to_file_uri(&expand_tilde(host, path)),
        }
    }

    /// If this is a remote path starting with `~` and the host's home directory is
    /// known, return the equivalent absolute path.
    pub fn resolve_remote_tilde(&self) -> Option<VPath> {
        let VPath::Remote { host, path } = self else { return None };
        let raw = path.to_string_lossy();
        if raw != "~" && !raw.starts_with("~/") { return None; }
        let home = remote_home(host)?;
        let rest = raw.strip_prefix('~').unwrap_or("").trim_start_matches('/');
        let abs = if rest.is_empty() { home } else { home.join(rest) };
        Some(VPath::Remote { host: host.clone(), path: abs })
    }

    /// Short human-readable display for status bar / command palette.
    pub fn display_short(&self) -> String {
        match self {
            VPath::Local(p) => p.display().to_string(),
            VPath::Remote { host, path } =>
                format!("[{}] {}", host.display(), path.display()),
        }
    }

    /// Display the parent directory as a string (for the quick-finder dir
    /// column).  Local paths use the native path separator; remote paths
    /// are prefixed with `[host]`.
    pub fn parent_str(&self) -> String {
        match self {
            VPath::Local(p) => p.parent()
                .and_then(|d| d.to_str())
                .unwrap_or("")
                .to_owned(),
            VPath::Remote { host, path } => {
                let dir = path.parent()
                    .and_then(|d| d.to_str())
                    .unwrap_or("");
                format!("[{}] {}", host.display(), dir)
            }
        }
    }
}

// ── Remote home directories ───────────────────────────────────────────────────

fn remote_homes() -> &'static std::sync::Mutex<std::collections::HashMap<SshHost, PathBuf>> {
    static HOMES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<SshHost, PathBuf>>> =
        std::sync::OnceLock::new();
    HOMES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Record the `$HOME` reported by `host` (read once after connecting).
pub fn set_remote_home(host: &SshHost, home: PathBuf) {
    remote_homes().lock().unwrap_or_else(|p| p.into_inner()).insert(host.clone(), home);
}

/// The host's home directory, if it has been read.
pub fn remote_home(host: &SshHost) -> Option<PathBuf> {
    remote_homes().lock().unwrap_or_else(|p| p.into_inner()).get(host).cloned()
}

/// Expand a leading `~` using the host's real home directory. Before that is known
/// we fall back to the conventional location (`/root` for root, `/home/<user>`
/// otherwise); if there's no user either, the path is returned unchanged.
fn expand_tilde(host: &SshHost, path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    let rest = if path_str == "~" {
        ""
    } else if let Some(rest) = path_str.strip_prefix("~/") {
        rest
    } else {
        return path.to_path_buf();
    };
    let home = match remote_home(host) {
        Some(home) => home,
        None => match host.user.as_deref() {
            Some("root") => PathBuf::from("/root"),
            Some(user)   => PathBuf::from(format!("/home/{user}")),
            None         => return path.to_path_buf(),
        },
    };
    if rest.is_empty() { home } else { home.join(rest) }
}

// ── file:// URIs ──────────────────────────────────────────────────────────────

/// Percent-encode a path into a `file://` URI. Everything outside the RFC 3986
/// unreserved set (plus `/`) is escaped, so spaces, `#`, `%`, `?` and non-ASCII
/// names survive the round trip through a language server.
pub fn path_to_file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decode a `file://` URI (as sent by a language server) back into a path.
/// Accepts `file:///abs` and `file://localhost/abs`; returns None for anything else.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') { return None; }
    // Drop any query/fragment (not part of a file path once decoded).
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    if out.contains(&0) { return None; }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Map a URI reported by a language server back to a VPath. For remote servers the
/// server sees absolute paths; if the matching open path uses `~`, the caller
/// compares via `VPath::same_file`.
pub fn lsp_uri_to_vpath(uri: &str, ssh_host: Option<&SshHost>) -> Option<VPath> {
    let path = file_uri_to_path(uri)?;
    Some(match ssh_host {
        Some(host) => VPath::Remote { host: host.clone(), path },
        None       => VPath::Local(path),
    })
}

impl VPath {
    /// True if both paths name the same file, treating a remote `~/x` and the
    /// absolute path under the host's home as equal.
    pub fn same_file(&self, other: &VPath) -> bool {
        if self == other { return true; }
        match (self, other) {
            (VPath::Remote { host: h1, path: p1 }, VPath::Remote { host: h2, path: p2 }) if h1 == h2 =>
                expand_tilde(h1, p1) == expand_tilde(h2, p2),
            _ => false,
        }
    }
}

fn split_ssh_host_and_path(rest: &str) -> Option<(&str, &str)> {
    if rest.is_empty() { return None; }

    let authority_start = rest.rfind('@').map(|idx| idx + 1).unwrap_or(0);
    let first_slash = rest[authority_start..].find('/').map(|idx| authority_start + idx);
    let first_colon = rest[authority_start..].find(':').map(|idx| authority_start + idx);

    if let Some(slash) = first_slash {
        if first_colon.map_or(true, |colon| slash < colon) {
            return Some((&rest[..slash], &rest[slash..]));
        }
    }

    if let Some(colon) = first_colon {
        if let Some(slash) = first_slash {
            if slash > colon + 1 {
                let maybe_port = &rest[colon + 1..slash];
                if maybe_port.parse::<u16>().is_ok() {
                    return Some((&rest[..slash], &rest[slash..]));
                }
            }
        }

        let after_colon = &rest[colon + 1..];
        if after_colon.is_empty() {
            return Some((&rest[..colon], "~"));
        }
        if after_colon.parse::<u16>().is_ok() {
            return Some((rest, "~"));
        }
        if let Some(second_colon) = after_colon.find(':') {
            let maybe_port = &after_colon[..second_colon];
            if maybe_port.parse::<u16>().is_ok() {
                let path = &after_colon[second_colon + 1..];
                return Some((&rest[..colon + 1 + second_colon], if path.is_empty() { "~" } else { path }));
            }
        }
        return Some((&rest[..colon], after_colon));
    }

    Some((rest, "~"))
}

// ── Trait impls ───────────────────────────────────────────────────────────────

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_short())
    }
}

impl From<PathBuf> for VPath {
    fn from(p: PathBuf) -> Self { VPath::Local(p) }
}

impl From<&Path> for VPath {
    fn from(p: &Path) -> Self { VPath::Local(p.to_path_buf()) }
}

impl PartialOrd for VPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Ord for VPath {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (VPath::Local(a),  VPath::Local(b))  => a.cmp(b),
            (VPath::Remote { host: h1, path: p1 }, VPath::Remote { host: h2, path: p2 }) => {
                h1.cmp(h2).then_with(|| p1.cmp(p2))
            }
            // Local sorts before Remote
            (VPath::Local(_),  VPath::Remote { .. }) => Ordering::Less,
            (VPath::Remote { .. }, VPath::Local(_))  => Ordering::Greater,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_remote(input: &str, user: Option<&str>, host: &str, port: Option<u16>, path: &str) {
        match VPath::parse(input) {
            VPath::Remote { host: parsed_host, path: parsed_path } => {
                assert_eq!(parsed_host.user.as_deref(), user);
                assert_eq!(parsed_host.host.as_str(), host);
                assert_eq!(parsed_host.port, port);
                assert_eq!(parsed_path, PathBuf::from(path));
            }
            other => panic!("expected remote path for {input:?}, got {other:?}"),
        }
    }

    #[test]
    fn parses_remote_uri_forms() {
        assert_remote("ssh://example.com", None, "example.com", None, "~");
        assert_remote("ssh://example.com:/srv/app", None, "example.com", None, "/srv/app");
        assert_remote("ssh://example.com:2222", None, "example.com", Some(2222), "~");
        assert_remote("ssh://example.com:2222:/srv/app", None, "example.com", Some(2222), "/srv/app");
        // `~` is kept until the real remote $HOME is known.
        assert_remote("ssh://me@example.com:~/src/app", Some("me"), "example.com", None, "~/src/app");
    }

    #[test]
    fn tilde_uses_resolved_home_then_falls_back() {
        let root = SshHost { user: Some("root".into()), host: "tilde-test-root".into(), port: None };
        let p = VPath::Remote { host: root.clone(), path: PathBuf::from("~/proj/a.rs") };
        assert_eq!(p.to_lsp_uri(), "file:///root/proj/a.rs");

        let mac = SshHost { user: Some("me".into()), host: "tilde-test-mac".into(), port: None };
        set_remote_home(&mac, PathBuf::from("/Users/me"));
        let p = VPath::Remote { host: mac.clone(), path: PathBuf::from("~/x") };
        assert_eq!(p.to_lsp_uri(), "file:///Users/me/x");
        assert_eq!(
            p.resolve_remote_tilde(),
            Some(VPath::Remote { host: mac.clone(), path: PathBuf::from("/Users/me/x") }),
        );
        assert!(p.same_file(&VPath::Remote { host: mac, path: PathBuf::from("/Users/me/x") }));
    }

    #[test]
    fn file_uris_round_trip_special_characters() {
        let p = Path::new("/tmp/My Project/a#b%c/é.rs");
        let uri = path_to_file_uri(p);
        assert_eq!(uri, "file:///tmp/My%20Project/a%23b%25c/%C3%A9.rs");
        assert_eq!(file_uri_to_path(&uri).as_deref(), Some(p));
        assert_eq!(file_uri_to_path("file://localhost/x/y").as_deref(), Some(Path::new("/x/y")));
        assert_eq!(file_uri_to_path("file://relative"), None);
    }

    #[test]
    fn preserves_colons_in_remote_path() {
        assert_remote("ssh://example.com:/srv/app:a/b:c", None, "example.com", None, "/srv/app:a/b:c");
        assert_remote("ssh://example.com:2222:~/src/app:a", None, "example.com", Some(2222), "~/src/app:a");
    }

    #[test]
    fn keeps_legacy_slash_path_form() {
        assert_remote("ssh://example.com/srv/app", None, "example.com", None, "/srv/app");
        assert_remote("ssh://example.com:2222/srv/app", None, "example.com", Some(2222), "/srv/app");
    }

    #[test]
    fn expands_remote_tilde_for_lsp_uri_when_user_is_known() {
        // No resolved home for this host → conventional fallback for a non-root user.
        let path = VPath::parse("ssh://me@example.com:~/src/app/main.ts");
        assert_eq!(path.to_lsp_uri(), "file:///home/me/src/app/main.ts");
    }
}

#[cfg(test)]
mod control_path_len_tests {
    use super::*;
    fn h(user: &str, host: &str) -> SshHost {
        SshHost { user: Some(user.into()), host: host.into(), port: None }
    }
    // A control socket path that exceeds the Unix-domain `sun_path` limit (104 bytes on
    // macOS) makes ssh fail with "ControlPath too long". The filename is a fixed-length
    // hash, so even an arbitrarily long hostname must not push us over the limit.
    #[test]
    fn socket_path_stays_under_sun_path_limit() {
        let cases = [
            h("clauder", "localhost"),
            h("clauder", "a-very-long-fully-qualified-domain-name.engineering.example.com"),
            h("some-long-username-here", "another.really.long.host.name.internal.corp.example.org"),
        ];
        for host in &cases {
            let p = host.control_path();
            let len = p.as_os_str().len();
            assert!(len < 104, "socket path too long ({len}): {}", p.display());
        }
    }
}
