// Persistent settings — serialized to/from ~/.config/local-text/config.json.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RendererBackend { Cpu, Gpu }

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TermWordSelect {
    #[default]
    Whitespace,  // bounded by whitespace (good for URLs, git branches)
    Word,        // alphanumeric + underscore (editor-style)
}

// Maximum number of entries in the glyph rasterization cache.
// Unlimited lets the cache grow without bound; Bounded(N) evicts non-ASCII
// glyphs when the entry count exceeds N. N must be a power of two >= 512.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum GlyphCacheLimit {
    #[default]
    #[serde(rename = "unlimited")]
    Unlimited,
    #[serde(rename = "512")]
    N512,
    #[serde(rename = "1024")]
    N1024,
    #[serde(rename = "2048")]
    N2048,
    #[serde(rename = "4096")]
    N4096,
}

impl GlyphCacheLimit {
    pub fn cap(self) -> Option<usize> {
        match self {
            Self::Unlimited => None,
            Self::N512  => Some(512),
            Self::N1024 => Some(1024),
            Self::N2048 => Some(2048),
            Self::N4096 => Some(4096),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Unlimited => "Unlimited",
            Self::N512      => "512",
            Self::N1024     => "1024",
            Self::N2048     => "2048",
            Self::N4096     => "4096",
        }
    }
    pub const ALL: [GlyphCacheLimit; 5] = [
        Self::Unlimited, Self::N512, Self::N1024, Self::N2048, Self::N4096,
    ];
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub renderer:         RendererBackend,
    pub vsync:            bool,
    #[serde(default = "Settings::default_font_size")]
    pub font_size:        f32,
    #[serde(default)]
    pub rainbow_brackets: bool,
    #[serde(default = "Settings::default_undo_limit")]
    pub undo_limit:       Option<usize>,
    #[serde(default)]
    pub term_copy_paste:  bool,
    #[serde(default)]
    pub term_cmd_bs:      bool,
    #[serde(default)]
    pub term_alt_bs:      bool,
    #[serde(default)]
    pub term_word_select: TermWordSelect,
    #[serde(default)]
    pub format_on_save:           String,  // comma-separated globs, e.g. "**/*.ts,**/*.rs"
    #[serde(default)]
    pub organize_imports_on_save: String,  // comma-separated globs
    #[serde(default)]
    pub format_command:           String,  // e.g. "rustfmt" or "prettier --write {file}"
    #[serde(default)]
    pub glyph_cache_limit: GlyphCacheLimit,
    #[serde(default = "Settings::default_cpu_double_buffer")]
    pub cpu_double_buffer: bool,   // true = 2 IOSurfaces (tear-free); false = legacy single-buffer
    #[serde(default = "Settings::default_gpu_drawable_count")]
    pub gpu_drawable_count: u8,    // Metal drawable pool size: 2 or 3 (Apple minimum is 2)
    /// Recently opened remote SSH URIs (most recent first), stored as "ssh://[user@]host[:port]:path".
    #[serde(default)]
    pub recent_remote_hosts: Vec<String>,
    /// Per-host extra PATH directories for finding LSP binaries on remote.
    /// Key = host string (e.g. "user@host" or "host"), value = list of directories.
    #[serde(default)]
    pub remote_lsp_search_paths: std::collections::HashMap<String, Vec<String>>,
    /// Machine-wide cap on how many local-text processes may run a given LSP
    /// server concurrently. Key = stable language id ("rust"/"typescript"/"python").
    /// A language **absent** from the map is **unlimited**; `n` = at most `n`
    /// instances; **`0` = none allowed (disabled)**. Counted per (lang, ssh host).
    #[serde(default)]
    pub lsp_server_limits: std::collections::HashMap<String, usize>,
    /// TCP port the host listens on for collab sessions (default 7777).
    #[serde(default = "Settings::default_collab_port")]
    pub collab_port: u16,
    /// Role permission matrix for collab sessions (which perms each role has).
    #[serde(default)]
    pub collab_role_perms: crate::collab::RolePermissions,
    /// Whitelist globs — if non-empty, only matching workspace paths are shared.
    #[serde(default)]
    pub collab_include_globs: Vec<String>,
    /// Blacklist globs — matching paths are never shared regardless of role.
    #[serde(default)]
    pub collab_exclude_globs: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            renderer:                 RendererBackend::Cpu,
            vsync:                    true,
            font_size:                Self::default_font_size(),
            rainbow_brackets:         false,
            undo_limit:               Self::default_undo_limit(),
            term_copy_paste:          false,
            term_cmd_bs:              false,
            term_alt_bs:              false,
            term_word_select:         TermWordSelect::Whitespace,
            format_on_save:           String::new(),
            organize_imports_on_save: String::new(),
            format_command:           String::new(),
            glyph_cache_limit:        GlyphCacheLimit::Unlimited,
            cpu_double_buffer:        Self::default_cpu_double_buffer(),
            gpu_drawable_count:       Self::default_gpu_drawable_count(),
            recent_remote_hosts:      Vec::new(),
            remote_lsp_search_paths:  std::collections::HashMap::new(),
            lsp_server_limits:        std::collections::HashMap::new(),
            collab_port:              Self::default_collab_port(),
            collab_role_perms:        crate::collab::RolePermissions::default(),
            collab_include_globs:     Vec::new(),
            collab_exclude_globs:     Vec::new(),
        }
    }
}

impl Settings {
    pub fn default_font_size() -> f32 { 14.0 }
    pub fn default_undo_limit() -> Option<usize> { Some(200) }
    pub fn default_cpu_double_buffer() -> bool { true }
    pub fn default_gpu_drawable_count() -> u8 { 2 }
    pub fn default_collab_port() -> u16 { 7777 }

    fn config_path() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        let dir  = PathBuf::from(home).join(".config").join("local-text");
        fs::create_dir_all(&dir).ok()?;
        Some(dir.join("config.json"))
    }

    pub fn load() -> Self {
        let path = match Self::config_path() { Some(p) => p, None => return Self::default() };
        let text = match fs::read_to_string(&path) { Ok(t) => t, Err(_) => return Self::default() };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::config_path() else { return };
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, text);
        }
    }
}
