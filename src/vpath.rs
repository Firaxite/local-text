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

impl SshHost {
    /// The ControlMaster socket path used to multiplex connections.
    pub fn control_path(&self) -> PathBuf {
        let tag = format!(
            "{}-{}-{}",
            self.user.as_deref().unwrap_or(""),
            self.host,
            self.port.unwrap_or(22),
        );
        PathBuf::from(format!("/tmp/local-text-ssh-{tag}"))
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
                return VPath::Remote {
                    host: SshHost { user, host, port },
                    path: PathBuf::from(path_part),
                };
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
        format!("file://{}", self.as_path().display())
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
        assert_remote("ssh://me@example.com:~/src/app", Some("me"), "example.com", None, "~/src/app");
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
}
