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

// `#[serde(default)]` on the container: any field missing from config.json (older
// file, hand edit) takes its value from `Settings::default()` instead of failing the
// whole parse and silently resetting every setting.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
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
    /// Program (+ arguments) a new terminal runs, e.g. `["bash", "--login"]` or on
    /// Windows `["C:\\Program Files\\Git\\bin\\bash.exe", "--login"]`, `["wsl.exe"]`.
    /// Empty = `$SHELL` (macOS/Linux) or PowerShell (Windows).
    #[serde(default)]
    pub terminal_shell:           Vec<String>,
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
    /// Project roots whose `.vscode/local-text.json` the user has trusted, mapped to
    /// the exact file contents that were trusted. A project config can choose which
    /// program runs as the language server, so it is only applied when its current
    /// contents match what the user approved.
    #[serde(default)]
    pub trusted_project_configs: std::collections::HashMap<String, String>,
    /// Width of a tab stop, and spaces per indent level when `insert_spaces`.
    pub tab_size:           usize,
    /// Tab key / auto-indent inserts spaces (true) or tab characters (false).
    pub insert_spaces:      bool,
    /// Detect tab size / tabs-vs-spaces per file from its contents when opened.
    pub detect_indentation: bool,
    /// Soft-wrap long lines at the window edge.
    pub word_wrap:          bool,
    /// Color theme: one of the built-in names (see `theme::BUILTIN`).
    pub theme:              String,
    /// Per-color overrides on top of the theme: field name (e.g. "bg", "accent",
    /// "hl_keyword", "ansi1") → "#RRGGBB".
    pub theme_overrides:    std::collections::HashMap<String, String>,
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
            terminal_shell:           Vec::new(),
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
            trusted_project_configs:  std::collections::HashMap::new(),
            tab_size:                 4,
            insert_spaces:            true,
            detect_indentation:       true,
            word_wrap:                false,
            theme:                    crate::theme::TOKYO_NIGHT.name.to_owned(),
            theme_overrides:          std::collections::HashMap::new(),
        }
    }
}

impl Settings {
    pub fn default_font_size() -> f32 { 14.0 }
    pub fn default_undo_limit() -> Option<usize> { Some(200) }
    pub fn default_cpu_double_buffer() -> bool { true }
    pub fn default_gpu_drawable_count() -> u8 { 2 }
    pub fn default_collab_port() -> u16 { 7777 }

    /// The configured default indentation (before per-file detection).
    pub fn default_indent(&self) -> crate::layout::Indent {
        crate::layout::Indent { use_tabs: !self.insert_spaces, size: self.tab_size.clamp(1, 16) }
    }

    /// `config.json` in the platform config directory (see `os::config_dir`).
    fn config_path() -> Option<PathBuf> {
        let dir = crate::os::config_dir()?;
        fs::create_dir_all(&dir).ok()?;
        Some(dir.join("config.json"))
    }

    pub fn load() -> Self {
        let path = match Self::config_path() { Some(p) => p, None => return Self::default() };
        let text = match fs::read_to_string(&path) { Ok(t) => t, Err(_) => return Self::default() };
        let (settings, fully_valid) = Self::parse_lenient(&text);
        if !fully_valid {
            // Keep the user's original file: the next save() rewrites config.json, so
            // stash a copy of what we couldn't fully understand instead of losing it.
            let backup = path.with_extension("json.bak");
            let _ = fs::write(&backup, &text);
            eprintln!(
                "local-text: some settings in {} were invalid and were reset; original saved to {}",
                path.display(), backup.display(),
            );
        }
        settings
    }

    /// Parse settings, keeping every field that is individually valid. Returns the
    /// settings plus whether the whole file parsed cleanly. A single bad value (wrong
    /// type, unknown enum variant) only resets that one field to its default.
    fn parse_lenient(text: &str) -> (Self, bool) {
        if let Ok(s) = serde_json::from_str::<Self>(text) {
            return (s, true);
        }
        let Ok(serde_json::Value::Object(user)) = serde_json::from_str::<serde_json::Value>(text) else {
            return (Self::default(), false);
        };
        let Ok(serde_json::Value::Object(mut merged)) = serde_json::to_value(Self::default()) else {
            return (Self::default(), false);
        };
        for (key, value) in user {
            let mut trial = merged.clone();
            trial.insert(key.clone(), value.clone());
            if serde_json::from_value::<Self>(serde_json::Value::Object(trial)).is_ok() {
                merged.insert(key, value);
            }
        }
        let settings = serde_json::from_value(serde_json::Value::Object(merged)).unwrap_or_default();
        (settings, false)
    }

    pub fn save(&self) {
        let Some(path) = Self::config_path() else { return };
        if let Ok(text) = serde_json::to_string_pretty(self) {
            // Write-then-rename so a crash mid-write can't leave a truncated config.
            let tmp = path.with_extension("json.tmp");
            if fs::write(&tmp, text).is_ok() && fs::rename(&tmp, &path).is_err() {
                let _ = fs::remove_file(&tmp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        let (s, ok) = Settings::parse_lenient(r#"{"font_size": 18.0}"#);
        assert!(ok);
        assert_eq!(s.font_size, 18.0);
        assert!(s.vsync); // default, even though "vsync"/"renderer" are absent
    }

    #[test]
    fn one_bad_field_does_not_reset_the_rest() {
        let (s, ok) = Settings::parse_lenient(
            r#"{"font_size": 20.0, "renderer": "quantum", "recent_remote_hosts": ["ssh://h:/x"]}"#,
        );
        assert!(!ok);
        assert_eq!(s.font_size, 20.0);
        assert_eq!(s.recent_remote_hosts, vec!["ssh://h:/x".to_owned()]);
        assert!(s.renderer == RendererBackend::Cpu);
    }
}
