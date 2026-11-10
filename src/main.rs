// Release builds on Windows are GUI programs: no console window behind the editor.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod os;
mod platform;
mod settings;
mod terminal;
mod lsp;
mod lsp_registry;
mod proj_config;
mod vpath;
mod ssh;
mod collab;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use vpath::VPath;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use fontdue::{Font, FontSettings, Metrics};
use ropey::Rope;
use winit::application::ApplicationHandler;
use std::time::{Duration, Instant};
#[cfg(feature = "logging")]
use std::time::{SystemTime, UNIX_EPOCH};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Window, WindowAttributes};
#[cfg(target_os = "macos")]
use winit::platform::macos::WindowAttributesExtMacOS;
#[cfg(unix)]
use libc;

#[cfg(feature = "logging")]
fn ts() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

macro_rules! dlog {
    ($($arg:tt)*) => {
        #[cfg(feature = "logging")]
        eprintln!($($arg)*);
    }
}

// ── TokyoNight palette (0x00RRGGBB) ──────────────────────────────────────────
const BG:     u32 = 0x1A1B26;
const BG2:    u32 = 0x24283B;
const FG:     u32 = 0xA9B1D6;
const FG_DIM: u32 = 0x565F89;
const ACCENT: u32 = 0x7AA2F7;
const BORDER: u32 = 0x3B4261;
const SEL_BG: u32 = 0x2D3149;

// ── Syntax highlight colors (TokyoNight) ─────────────────────────────────────
const HL_KEYWORD: u32 = 0xBB9AF7;
const HL_STRING:  u32 = 0x9ECE6A;
const HL_NUMBER:  u32 = 0xFF9E64;
const HL_COMMENT: u32 = 0x565F89;
const HL_TYPE:    u32 = 0x2AC3DE;
const HL_FUNC:    u32 = 0x7AA2F7;

// ── Find highlight colors ─────────────────────────────────────────────────────
const HL_MATCH:        u32 = 0x3D3557; // subtle purple — inactive find matches
const HL_MATCH_ACTIVE: u32 = 0x524175; // brighter purple — active match

// ── Git diff colors ───────────────────────────────────────────────────────────
const DIFF_ADD_FG: u32 = 0x9ECE6A;  // green  – added line text
const DIFF_DEL_FG: u32 = 0xF7768E;  // red    – removed line text
const DIFF_ADD_BG: u32 = 0x20322E;  // dark green tint – added line background
const DIFF_DEL_BG: u32 = 0x32201E;  // dark red tint   – removed line background
const DIFF_HUNK:   u32 = 0x7DCFFF;  // cyan   – @@ hunk header

// ── Rainbow bracket colors (depth-cycled) ────────────────────────────────────
const RAINBOW: [u32; 6] = [0xFF79C6, 0xFFB86C, 0xF1FA8C, 0x50FA7B, 0x8BE9FD, 0xBD93F9];

// ── Language detection ────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Lang { None, Rust, Python, TypeScript, Json, Jsonc, Markdown, Css, Html }

impl Lang {
    fn from_path(path: &std::path::Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs")                              => Lang::Rust,
            Some("py" | "pyw")                      => Lang::Python,
            Some("ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs") => Lang::TypeScript,
            Some("json")                            => Lang::Json,
            Some("jsonc")                           => Lang::Jsonc,
            Some("md" | "markdown")                 => Lang::Markdown,
            Some("css" | "less" | "scss")           => Lang::Css,
            Some("html" | "htm" | "svg" | "xml")    => Lang::Html,
            _                                       => Lang::None,
        }
    }

    /// Stable lowercase identifier used as a cross-process / on-disk key
    /// (registry entries, `lsp_server_limits` settings keys). Independent of the
    /// enum's declaration order so reordering can't corrupt persisted data.
    fn as_registry_str(self) -> Option<&'static str> {
        match self {
            Lang::Rust       => Some("rust"),
            Lang::TypeScript => Some("typescript"),
            Lang::Python     => Some("python"),
            Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown | Lang::Css | Lang::Html => None,
        }
    }

    fn from_registry_str(s: &str) -> Option<Lang> {
        match s {
            "rust"       => Some(Lang::Rust),
            "typescript" => Some(Lang::TypeScript),
            "python"     => Some(Lang::Python),
            _            => None,
        }
    }

    fn from_path_and_first_line(path: Option<&std::path::Path>, first_line: &str) -> Self {
        if let Some(path_lang) = path.map(Self::from_path).filter(|lang| *lang != Lang::None) {
            return path_lang;
        }
        let Some(shebang) = first_line.trim_end_matches(['\r', '\n']).strip_prefix("#!") else {
            return Lang::None;
        };
        let is_python = shebang
            .split(|c: char| c == '/' || c.is_ascii_whitespace())
            .filter(|part| !part.is_empty())
            .any(|part| {
                let part = part.to_ascii_lowercase();
                let Some(suffix) = part.strip_prefix("python") else { return false };
                suffix.is_empty() || suffix.starts_with(|c: char| c.is_ascii_digit())
            });
        if is_python { Lang::Python } else { Lang::None }
    }
}

fn python_line_opens_block(line_before_cursor: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut code_end = line_before_cursor.len();
    for (idx, ch) in line_before_cursor.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if matches!(ch, '\'' | '"') => quote = Some(ch),
            None if ch == '#' => {
                code_end = idx;
                break;
            }
            None => {}
        }
    }
    line_before_cursor[..code_end].trim_end().ends_with(':')
}

// ── Multi-line tokenizer state ────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum MlState {
    Normal,
    BlockComment,
    TemplateStr,
    PyTripleSingle,
    PyTripleDouble,
    CodeFence,
    HtmlComment,  // <!-- ... -->  used by HTML/SVG/XML
    HtmlTag,      // inside < tag attrs > used by HTML and JSX/TSX
}

// ── Layout ────────────────────────────────────────────────────────────────────
const FONT_PX:  f32 = 14.0;
const ED_LPAD:  i32 = 6;
const SB_W:       i32 = 6;
const SB_THUMB:   u32 = 0x414868;

// ── Glyph cache ───────────────────────────────────────────────────────────────

// Fallback fonts tried in order when a character isn't in the primary font: a
// symbols font, then a math font. Each slot lists alternative locations (distros
// install the same font in different places); the first that exists is used.
// Fonts are loaded lazily on first use to avoid paying ~43 MiB of heap at startup
// for font outlines (fontdue parses all glyphs eagerly in Font::from_bytes).
#[cfg(target_os = "macos")]
const FALLBACK_FONTS: &[&[&str]] = &[
    &["/System/Library/Fonts/Apple Symbols.ttf"],
    &["/System/Library/Fonts/Symbol.ttf"],
    &["/System/Library/Fonts/Supplemental/STIXTwoMath.otf"],
];
#[cfg(windows)]
const FALLBACK_FONTS: &[&[&str]] = &[
    &[r"%WINDIR%\Fonts\seguisym.ttf"],  // Segoe UI Symbol
    &[r"%WINDIR%\Fonts\symbol.ttf"],
    &[r"%WINDIR%\Fonts\cambria.ttc"],   // Cambria Math
];
#[cfg(all(unix, not(target_os = "macos")))]
const FALLBACK_FONTS: &[&[&str]] = &[
    &[
        "/usr/share/fonts/TTF/DejaVuSans.ttf",                      // Arch
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",          // Debian/Ubuntu
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",        // Fedora
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    ],
    &[
        "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/google-noto/NotoSansSymbols2-Regular.ttf",
    ],
    &[
        "/usr/share/fonts/noto/NotoSansMath-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansMath-Regular.ttf",
        "/usr/share/fonts/google-noto/NotoSansMath-Regular.ttf",
    ],
];

/// A fallback font slot: not looked at yet, absent on this system, or loaded.
#[derive(Clone)]
enum FallbackFont { Unloaded, Missing, Loaded(Font) }

impl FallbackFont {
    fn load(candidates: &[&str]) -> Self {
        candidates.iter()
            .filter_map(|p| std::fs::read(expand_env_vars(p)).ok())
            .find_map(|bytes| Font::from_bytes(bytes, FontSettings::default()).ok())
            .map_or(FallbackFont::Missing, FallbackFont::Loaded)
    }
}

/// Expand `%NAME%` (Windows-style) environment references in a font path.
fn expand_env_vars(path: &str) -> PathBuf {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else { break };
        let name = &rest[start + 1..start + 1 + len];
        out.push_str(&rest[..start]);
        match std::env::var(name) {
            Ok(v) => out.push_str(&v),
            Err(_) if name.eq_ignore_ascii_case("WINDIR") => out.push_str(r"C:\Windows"),
            Err(_) => { out.push('%'); out.push_str(name); out.push('%'); }
        }
        rest = &rest[start + 2 + len..];
    }
    out.push_str(rest);
    PathBuf::from(out)
}

struct Glyphs {
    font:       Font,
    fallbacks:  Vec<FallbackFont>,  // indexed parallel to FALLBACK_FONTS
    px:         f32,
    map:        HashMap<char, (Metrics, Vec<u8>)>,
    pub cw:     i32,
    pub lh:     i32,
    pub asc:    i32,
    pub max_entries: Option<usize>,
}

impl Glyphs {
    fn new(bytes: &[u8], px: f32) -> Self {
        let font = Font::from_bytes(bytes, FontSettings::default()).unwrap();
        let mut s = Self { font, fallbacks: vec![FallbackFont::Unloaded; FALLBACK_FONTS.len()],
                           px, map: HashMap::new(), cw: 0, lh: 0, asc: 0, max_entries: None };
        s.rebuild_cache(px);
        s
    }

    fn resize(&mut self, px: f32) {
        self.map.clear();
        self.rebuild_cache(px);
    }

    fn rebuild_cache(&mut self, px: f32) {
        self.px = px;
        let (m, _) = self.font.rasterize('M', px);
        self.cw  = m.advance_width.ceil() as i32;
        self.lh  = (px * 1.5).ceil() as i32;
        self.asc = (px * 1.1).ceil() as i32;
        for ch in ' '..='~' { self.load(ch); }
        for ch in ['▶', '▼', '•', '×', '⚙'] { self.load(ch); }
    }

    fn load(&mut self, ch: char) {
        if self.map.contains_key(&ch) { return; }
        // Try primary font first
        if self.font.lookup_glyph_index(ch) != 0 {
            let (m, b) = self.font.rasterize(ch, self.px);
            self.map.insert(ch, (m, b));
            self.evict_if_over_cap();
            return;
        }
        // Try fallback fonts; load lazily on first use (and only once: a font
        // that isn't installed is remembered as missing, not re-read every frame)
        for i in 0..FALLBACK_FONTS.len() {
            if matches!(self.fallbacks[i], FallbackFont::Unloaded) {
                self.fallbacks[i] = FallbackFont::load(FALLBACK_FONTS[i]);
            }
            if let FallbackFont::Loaded(f) = &self.fallbacks[i] {
                if f.lookup_glyph_index(ch) != 0 {
                    let (m, b) = f.rasterize(ch, self.px);
                    self.map.insert(ch, (m, b));
                    self.evict_if_over_cap();
                    return;
                }
            }
        }
        // Character not found in any font — leave absent so render skips it
    }

    fn evict_if_over_cap(&mut self) {
        if let Some(cap) = self.max_entries {
            if self.map.len() > cap {
                self.map.retain(|&c, _| c <= '\u{007E}');
            }
        }
    }

    fn get(&self, ch: char) -> Option<(&Metrics, &[u8])> {
        self.map.get(&ch).map(|(m, b)| (m, b.as_slice()))
    }
}

// ── Cursor ────────────────────────────────────────────────────────────────────
#[derive(Clone)]
struct Cursor {
    head: usize, // moving end
    tail: usize, // fixed end (selection anchor)
}

impl Cursor {
    const fn new(pos: usize) -> Self { Cursor { head: pos, tail: pos } }
    fn lo(&self) -> usize { self.head.min(self.tail) }
    fn hi(&self) -> usize { self.head.max(self.tail) }
    fn has_sel(&self) -> bool { self.head != self.tail }
    fn sel(&self) -> Option<(usize, usize)> {
        if self.has_sel() { Some((self.lo(), self.hi())) } else { None }
    }
}

// ── Undo history ─────────────────────────────────────────────────────────────
#[derive(Clone)]
struct UndoEntry {
    text:    Rope,
    cursors: Vec<Cursor>,
}

// ── Find bar ──────────────────────────────────────────────────────────────────
#[derive(PartialEq, Clone, Copy)]
enum FindFocus { Query, Replace }

struct FindBar {
    open:           bool,
    input_focused:  bool,
    replace_open:   bool,
    query:          String,
    replace:        String,
    case_sensitive: bool,
    whole_word:     bool,
    focus:          FindFocus,
    cursor_query:   usize,
    cursor_replace: usize,
    sel_anchor_q:   Option<usize>,
    sel_anchor_r:   Option<usize>,
    // Cached match positions — recomputed only when query/flags/file changes.
    match_cache:     Vec<(usize, usize)>,
    match_cache_gen: u64,                    // edit_generation when cache was built
    match_cache_key: (String, bool, bool),   // (query, case_sensitive, whole_word)
}

impl FindBar {
    fn new() -> Self {
        FindBar {
            open: false, input_focused: false, replace_open: false,
            query: String::new(), replace: String::new(),
            case_sensitive: false, whole_word: false,
            focus: FindFocus::Query,
            cursor_query: 0, cursor_replace: 0,
            sel_anchor_q: None, sel_anchor_r: None,
            match_cache: Vec::new(), match_cache_gen: 0,
            match_cache_key: (String::new(), false, false),
        }
    }
}

// ── Left panel / activity bar ─────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum LeftView { FileTree, GlobalSearch, Diagnostics, Git, Collab }

#[derive(Clone)]
pub struct GitEntry {
    xy:   (char, char),  // X=staged col, Y=working-tree col from --porcelain
    path: String,
}

#[derive(Clone, PartialEq)]
enum GitSel { None, Staged(usize), Unstaged(usize) }

struct GitPanel {
    staged:         Vec<GitEntry>,
    unstaged:       Vec<GitEntry>,
    commit_msg:     String,
    commit_cursor:  usize,
    commit_focused: bool,
    sel:            GitSel,
    is_git_repo:    bool,
    remote_error:   Option<String>,  // SSH/git error message from last remote refresh
    loading:        bool,
    scroll:         usize,
    /// Repository top level (status paths are relative to it).
    repo_root:      Option<VPath>,
}

impl GitPanel {
    fn new() -> Self {
        GitPanel {
            staged: vec![], unstaged: vec![],
            commit_msg: String::new(), commit_cursor: 0,
            commit_focused: false, sel: GitSel::None,
            is_git_repo: true, remote_error: None, loading: false, scroll: 0,
            repo_root: None,
        }
    }
}

#[derive(Clone)]
enum DiffLine {
    Hunk(String),     // "@@ -x,y +a,b @@" header
    Added(String),    // line content after leading '+'
    Removed(String),  // line content after leading '-'
    Context(String),  // unchanged context line
}

struct GitDiffTabData {
    path:    String,
    staged:  bool,
    lines:   Vec<DiffLine>,
    loading: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum CtxAction {
    Separator,
    OpenSettings,
    GotoDefinition,
    FindReferences,
    FormatDocument,
    OrganizeImports,
    Copy, Cut, Paste,
    CopyDiagnostic,
    TabCopyRelPath, TabCopyFullPath,
    TabOpenPreview,
    TabClose,
    TabSplitRight, TabSplitLeft, TabSplitDown, TabSplitUp,
    ExplorerOpenAsRoot, ExplorerNewFile, ExplorerNewFolder,
    ExplorerCopy, ExplorerPaste, ExplorerDuplicate, ExplorerRename,
    OpenTerminalHere,
    GitOpenFile, GitViewDiff, GitStage, GitUnstage,
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsFieldId { FormatOnSave, OrganizeImportsOnSave, FormatCommand, CollabPort, CollabIncludeGlobs, CollabExcludeGlobs }

#[derive(Clone)]
struct ContextMenuItem {
    label:    &'static str,
    shortcut: &'static str,
    action:   CtxAction,
    enabled:  bool,
}

struct ContextMenu {
    x:          i32,
    y:          i32,
    items:      Vec<ContextMenuItem>,
    hovered:    usize,
    tab_source: Option<(usize, usize)>,  // (pane_id, tab_idx) for tab bar menus
    git_entry:  Option<(bool, String)>,  // (staged, path)
    explorer_entry: Option<VPath>,
    explorer_entry_is_dir: bool,         // true when explorer_entry is a directory
    diag_message: Option<String>,
}

// ── Quick file finder ─────────────────────────────────────────────────────────
enum QfCommand {
    Static(usize),                                // index into COMMANDS
    LspOutput { pane_id: usize, title: String },  // one per running LSP server
}

struct QuickFinder {
    open:               bool,
    query:              String,
    cursor:             usize,
    sel_anchor:         Option<usize>,
    entries:            Vec<VPath>,
    filtered:           Vec<usize>,
    filtered_commands:  Vec<QfCommand>,
    selected:           usize,
    walk_token:         u64,   // incremented on each open; stale QuickFinderFiles events are dropped
    loading:            bool,  // true while background walk_files is in progress
    remote_directory_mode: bool,
    /// Guest in a collab session: entries are the host's shared files
    /// (workspace-relative), opened through the session instead of from disk.
    collab_mode: bool,
    // line/col parsed from "file:line:col" query suffix (0-indexed)
    parsed_line:        Option<usize>,
    parsed_col:         Option<usize>,
    // synthetic entry for absolute/~ path queries
    direct_path:        Option<VPath>,
    // ':' goto-line mode: live-preview state for Escape restoration
    preview_pane_id:    Option<usize>,
    preview_tab_idx:    Option<usize>,
    preview_saved_char: Option<usize>,
}

// ── LSP manager panel ─────────────────────────────────────────────────────────
/// Modal overlay listing every LSP server running across all local-text
/// processes (from the cross-process registry), with actions to navigate to the
/// owning window, move a server to this window's project, or stop it.
struct LspManagerPanel {
    entries:  Vec<lsp_registry::LspRegistryEntry>,
    selected: usize,
}

// ── Command palette ───────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
enum CommandAction {
    Save, CloseTab, SplitRight, SplitDown, SplitLeft, SplitUp, OpenTerminal, GoToSettings,
    ToggleFind, ToggleReplace, ToggleExplorer, ToggleSidebar, IncreaseFontSize, DecreaseFontSize,
    GotoDefinition, FindReferences,
    Copy, Cut, Paste,
    CursorBack, CursorForward,
    FormatDocument, OrganizeImports,
    OpenMarkdownPreview,
    OpenRemoteDirectory,
    StartCollab,
    JoinCollab,
    ManageLspServers,
    EditProjectLspConfig,
    TrustProjectLspConfig,
}

struct CommandEntry { name: &'static str, shortcut: &'static str, action: CommandAction }

const COMMANDS: &[CommandEntry] = &[
    CommandEntry { name: "Save File",             shortcut: "Cmd+S",         action: CommandAction::Save },
    CommandEntry { name: "Close Tab",             shortcut: "Cmd+W",         action: CommandAction::CloseTab },
    CommandEntry { name: "Open Settings",         shortcut: "",              action: CommandAction::GoToSettings },
    CommandEntry { name: "Navigate Back",         shortcut: "Cmd+,",         action: CommandAction::CursorBack },
    CommandEntry { name: "Navigate Forward",      shortcut: "Cmd+.",         action: CommandAction::CursorForward },
    CommandEntry { name: "Open Terminal",         shortcut: "Ctrl+`",        action: CommandAction::OpenTerminal },
    CommandEntry { name: "Split Right",           shortcut: "",              action: CommandAction::SplitRight },
    CommandEntry { name: "Split Down",            shortcut: "",              action: CommandAction::SplitDown },
    CommandEntry { name: "Split Left",            shortcut: "",              action: CommandAction::SplitLeft },
    CommandEntry { name: "Split Up",              shortcut: "",              action: CommandAction::SplitUp },
    CommandEntry { name: "Toggle Find",           shortcut: "Cmd+F",         action: CommandAction::ToggleFind },
    CommandEntry { name: "Toggle Find+Replace",   shortcut: "Cmd+H",         action: CommandAction::ToggleReplace },
    CommandEntry { name: "Toggle File Explorer",  shortcut: "",              action: CommandAction::ToggleExplorer },
    CommandEntry { name: "Toggle Sidebar",        shortcut: "Cmd+B",         action: CommandAction::ToggleSidebar },
    CommandEntry { name: "Increase Font Size",    shortcut: "Cmd+=",         action: CommandAction::IncreaseFontSize },
    CommandEntry { name: "Decrease Font Size",    shortcut: "Cmd+-",         action: CommandAction::DecreaseFontSize },
    CommandEntry { name: "Go to Definition",      shortcut: "F12",           action: CommandAction::GotoDefinition },
    CommandEntry { name: "Find All References",   shortcut: "Cmd+Shift+F12", action: CommandAction::FindReferences },
    CommandEntry { name: "Format Document",       shortcut: "Opt+Shift+F",   action: CommandAction::FormatDocument },
    CommandEntry { name: "Organize Imports",      shortcut: "Opt+Shift+O",   action: CommandAction::OrganizeImports },
    CommandEntry { name: "Open Markdown Preview", shortcut: "Cmd+Shift+M",   action: CommandAction::OpenMarkdownPreview },
    CommandEntry { name: "Open Remote Directory…", shortcut: "",              action: CommandAction::OpenRemoteDirectory },
    CommandEntry { name: "Start Collab Session",   shortcut: "",              action: CommandAction::StartCollab },
    CommandEntry { name: "Join Collab Session…",   shortcut: "",              action: CommandAction::JoinCollab },
    CommandEntry { name: "LSP: Manage Servers",    shortcut: "",              action: CommandAction::ManageLspServers },
    CommandEntry { name: "LSP: Edit Project Settings", shortcut: "",          action: CommandAction::EditProjectLspConfig },
    CommandEntry { name: "LSP: Trust Project Settings", shortcut: "",         action: CommandAction::TrustProjectLspConfig },
];

// ── Global find/replace ───────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum GlobalFindFocus { Query, Replace, Include, Exclude, Results }

#[derive(Clone)]
struct GlobalFindResult {
    path:      VPath,
    line_num:  usize,
    line_text: String,
    match_col: usize,
    match_len: usize,
}

struct GlobalFind {
    query:          String,
    replace:        String,
    include_glob:   String,
    exclude_glob:   String,
    results:        Vec<GlobalFindResult>,
    scroll:         usize,
    selected:       usize,
    focus:          GlobalFindFocus,
    case_sensitive: bool,
    live_search:    bool,
    search_fire_at: Option<Instant>,
    searching:      bool,   // true while a background search thread is running
    search_token:   u64,    // incremented per search; stale SearchDone events are dropped
    cursor_query:   usize,
    cursor_replace: usize,
    cursor_include: usize,
    cursor_exclude: usize,
    sel_anchor_q:   Option<usize>,
    sel_anchor_r:   Option<usize>,
    sel_anchor_inc: Option<usize>,
    sel_anchor_exc: Option<usize>,
}

// ── Tab (per-file state) ──────────────────────────────────────────────────────
#[derive(Clone, PartialEq)]
enum TabKind { Editor, Settings, GitDiff }

#[derive(Clone)]
struct Tab {
    kind:         TabKind,
    buf_id:       usize,   // shared with sibling tabs showing the same buffer
    text:         Rope,
    path:         Option<VPath>,
    dirty:        bool,
    cursors:      Vec<Cursor>, // always non-empty; primary = last element
    scroll:       usize,
    hscroll:      usize,
    undo_stack:   Vec<UndoEntry>,
    redo_stack:   Vec<UndoEntry>,
    last_typing:  bool, // for coalescing consecutive single-char inserts
    hl_cache:      Vec<(MlState, i32)>, // (state, bracket_depth) at start of each line
    hl_dirty_from: usize,               // first line index needing hl_cache recompute
    /// Highlight colors for the lines currently on screen only (keyed by line).
    /// Pruned to the viewport every frame so RAM doesn't scale with file size.
    hl_color_cache: HashMap<usize, Vec<u32>>,
    /// Text before the first edit since the last render; the renderer diffs it
    /// against `text` to invalidate highlighting only from the first changed line.
    hl_before:      Option<Rope>,
    max_line_len:   Option<usize>, // cached max line length (chars), None = needs recompute
    edit_generation: u64, // incremented on every text mutation; used to skip redundant syncs
    /// edit_generation last sent to the language server (skip no-op didChange).
    lsp_synced_gen: Option<u64>,
    remote_load_token: Option<u64>,
    /// Workspace-relative path of a document shared by a collab host (guest side
    /// only). Such tabs have no `path`: the content lives on the host.
    collab_path: Option<String>,
}

impl Tab {
    fn untitled(buf_id: usize) -> Self {
        Tab { kind: TabKind::Editor, buf_id, text: Rope::new(), path: None, dirty: false,
              cursors: vec![Cursor::new(0)], scroll: 0, hscroll: 0,
              undo_stack: Vec::new(), redo_stack: Vec::new(), last_typing: false,
              hl_cache: Vec::new(), hl_dirty_from: 0, hl_color_cache: HashMap::new(),
              hl_before: None, max_line_len: Some(0), edit_generation: 0,
              lsp_synced_gen: None, remote_load_token: None, collab_path: None }
    }

    fn settings() -> Self {
        let mut tab = Tab::untitled(usize::MAX);
        tab.kind = TabKind::Settings;
        tab
    }

    fn display_name(&self) -> &str {
        match self.kind {
            TabKind::Settings => "Settings",
            TabKind::GitDiff  => self.path.as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("Diff"),
            TabKind::Editor   => self.path.as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .or_else(|| self.collab_path.as_deref().map(|p| p.rsplit('/').next().unwrap_or(p)))
                .unwrap_or("Untitled"),
        }
    }

    fn is_empty_untitled(&self) -> bool {
        self.kind == TabKind::Editor && self.path.is_none() && self.collab_path.is_none()
            && self.text.len_chars() == 0
    }

    fn primary(&self) -> &Cursor {
        // Invariant: cursors is never empty (primary_mut restores it). Fall back rather
        // than unwrap so a violated invariant can't panic a release build mid-render.
        const FALLBACK: Cursor = Cursor::new(0);
        self.cursors.last().unwrap_or(&FALLBACK)
    }
    fn primary_mut(&mut self) -> &mut Cursor {
        if self.cursors.is_empty() { self.cursors.push(Cursor::new(0)); }
        self.cursors.last_mut().unwrap()
    }

    fn sel(&self) -> Option<(usize, usize)> { self.primary().sel() }

    fn sel_text(&self) -> Option<String> {
        self.sel().map(|(lo, hi)| self.text.slice(lo..hi).chars().collect())
    }

    fn lang(&self) -> Lang {
        // Called every frame: check the extension first, and only look at the start
        // of line 0 for a shebang (never copy a multi-megabyte minified line).
        let path = self.path.as_ref().map(|path| path.as_path());
        if let Some(lang) = path.map(Lang::from_path).filter(|l| *l != Lang::None) {
            return lang;
        }
        let first_line = self.text.get_line(0)
            .map(|line| line.chars().take(256).collect::<String>())
            .unwrap_or_default();
        Lang::from_path_and_first_line(path, &first_line)
    }

    /// Record that the text is about to change: bumps the edit generation and keeps
    /// a snapshot of the pre-edit text (an O(1) Rope clone) so the renderer can
    /// work out which lines changed and only re-highlight from there.
    fn note_edit(&mut self) {
        if self.hl_before.is_none() {
            self.hl_before = Some(self.text.clone());
        }
        self.edit_generation += 1;
    }

    /// Push an undo snapshot for an edit that doesn't go through `State::push_undo`
    /// (background edits applied to a tab that may not be active).
    fn push_undo_snapshot(&mut self, limit: Option<usize>) {
        if let Some(lim) = limit {
            if self.undo_stack.len() >= lim && !self.undo_stack.is_empty() { self.undo_stack.remove(0); }
        }
        self.undo_stack.push(UndoEntry { text: self.text.clone(), cursors: self.cursors.clone() });
        self.redo_stack.clear();
        self.last_typing = false;
    }

    /// Replace the whole text (reload, remote content, collab snapshot), keeping
    /// cursors in range.
    fn replace_text(&mut self, text: Rope) {
        self.note_edit();
        self.text = text;
        self.clamp_cursors();
    }

    fn clamp_cursors(&mut self) {
        let len = self.text.len_chars();
        for c in &mut self.cursors {
            c.head = c.head.min(len);
            c.tail = c.tail.min(len);
        }
        if self.cursors.is_empty() { self.cursors.push(Cursor::new(0)); }
    }

    fn load_file(&mut self, path: VPath) -> Result<(), LoadError> {
        // Remote files are loaded asynchronously; this only records the path.
        let Some(local_path) = path.as_local_path() else {
            self.path = Some(path);
            self.cursors = vec![Cursor::new(0)];
            self.scroll = 0;
            self.hscroll = 0;
            self.dirty = false;
            self.undo_stack.clear();
            self.redo_stack.clear();
            self.last_typing = false;
            self.hl_cache.clear();
            self.hl_dirty_from = 0;
            self.hl_color_cache.clear();
            self.hl_before = None;
            self.max_line_len = Some(0);
            self.remote_load_token = None;
            return Err(LoadError::Remote);
        };
        const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
        match std::fs::metadata(local_path) {
            Ok(m) if m.len() > MAX_FILE_BYTES => return Err(LoadError::TooLarge),
            Ok(m) if m.is_dir() => return Err(LoadError::Io("is a directory".to_owned())),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LoadError::NotFound),
            Err(e) => return Err(LoadError::Io(e.to_string())),
        }
        let bytes = std::fs::read(local_path).map_err(|e| LoadError::Io(e.to_string()))?;
        // Refuse non-UTF-8 rather than load it lossily: saving would then silently
        // rewrite every invalid byte as U+FFFD.
        let content = String::from_utf8(bytes).map_err(|_| LoadError::NotUtf8)?;
        self.text       = Rope::from_str(&content);
        self.path       = Some(path);
        self.cursors    = vec![Cursor::new(0)];
        self.scroll     = 0;
        self.hscroll    = 0;
        self.dirty      = false;
        self.undo_stack = Vec::new();
        self.redo_stack = Vec::new();
        self.last_typing = false;
        self.hl_cache.clear();
        self.hl_dirty_from = 0;
        self.hl_color_cache.clear();
        self.hl_before = None;
        self.max_line_len = None;
        self.remote_load_token = None;
        self.edit_generation += 1;
        Ok(())
    }

    /// Start a new, not-yet-existing file at `path` (empty buffer; the first save creates it).
    fn start_new_file(&mut self, path: VPath) {
        self.text = Rope::new();
        self.path = Some(path);
        self.cursors = vec![Cursor::new(0)];
        self.dirty = false;
        self.hl_cache.clear();
        self.hl_dirty_from = 0;
        self.hl_color_cache.clear();
        self.hl_before = None;
        self.max_line_len = Some(0);
        self.edit_generation += 1;
    }

    /// Save to disk. Only editor tabs with a path can be saved: a git-diff tab's
    /// `path` names the file being diffed and its buffer is empty, so writing it
    /// would wipe that file. Remote saves complete asynchronously
    /// (`RemoteWriteDone`), which is why the edit generation travels with them.
    fn save_with_proxy(&mut self, proxy: Option<&winit::event_loop::EventLoopProxy<UserEvent>>) -> Result<(), String> {
        if self.kind != TabKind::Editor { return Err("This tab can't be saved".to_owned()); }
        if self.collab_path.is_some() {
            return Err("Shared file — the session host saves it".to_owned());
        }
        let Some(path) = &self.path else { return Err("Untitled file has no path".to_owned()) };
        let content: String = self.text.chunks().collect();
        match path {
            VPath::Local(local) => {
                write_file_atomically(local, content.as_bytes())
                    .map_err(|e| format!("Save failed ({}): {e}", local.display()))?;
                self.dirty = false;
                Ok(())
            }
            VPath::Remote { host, path: remote_path } => {
                let Some(proxy) = proxy else { return Err("remote save requires an event proxy".to_owned()) };
                ssh::ssh_write_file(host.clone(), remote_path.clone(), content, self.edit_generation, proxy.clone());
                // dirty is cleared on RemoteWriteDone if nothing changed in the meantime
                Ok(())
            }
        }
    }
}

/// Why a local file couldn't be opened.
#[derive(Debug)]
enum LoadError {
    Remote,      // remote path: loaded asynchronously, not an error
    TooLarge,
    NotUtf8,
    NotFound,
    Io(String),
}

impl LoadError {
    fn message(&self, path: &VPath) -> String {
        match self {
            LoadError::Remote   => format!("Loading {path}…"),
            LoadError::TooLarge => format!("File too large to open (>256 MB): {path}"),
            LoadError::NotUtf8  => format!("Can't open {path}: not valid UTF-8 text (binary file?)"),
            LoadError::NotFound => format!("File not found: {path}"),
            LoadError::Io(e)    => format!("Can't open {path}: {e}"),
        }
    }
}

/// Replace `path` with `content` without ever leaving a truncated file behind:
/// write a temp file in the same directory, fsync it, then rename it over the
/// target. Symlinks are resolved so the link's target is updated (the link stays a
/// link), and the original permissions are kept. Files with several hard links,
/// or that belong to another user, are rewritten in place instead, since a rename
/// would split the links or change the owner. On Windows, a rename that the OS
/// refuses (the file is open elsewhere without delete sharing) also falls back to
/// rewriting in place.
fn write_file_atomically(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let target = os::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let meta = std::fs::metadata(&target).ok();
    #[cfg(unix)]
    let in_place = meta.as_ref().map_or(false, |m| {
        use std::os::unix::fs::MetadataExt;
        m.nlink() > 1 || m.uid() != unsafe { libc::geteuid() }
    });
    // Windows: a read-only file can't be replaced by rename, and a read-only temp
    // copy couldn't even be cleaned up — write in place (which reports the error).
    #[cfg(not(unix))]
    let in_place = meta.as_ref().is_some_and(|m| m.permissions().readonly());
    if in_place {
        return std::fs::write(&target, content);
    }
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.lt-save-{}-{n}", std::process::id()));
    let mut renaming = false;
    let attempt = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        drop(f); // Windows can't rename a file that is still open
        if let Some(m) = &meta {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        renaming = true;
        std::fs::rename(&tmp, &target)
    })();
    match attempt {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            // e.g. the directory isn't writable but the file is: fall back to in place.
            if e.kind() == std::io::ErrorKind::PermissionDenied || (cfg!(windows) && renaming) {
                std::fs::write(&target, content)
            } else {
                Err(e)
            }
        }
    }
}

// ── File explorer ─────────────────────────────────────────────────────────────
#[derive(Clone, PartialEq, Eq)]
struct FileEntry {
    name:     String,
    path:     PathBuf,
    is_dir:   bool,
    expanded: bool,
    depth:    usize,
}

struct FileExplorer {
    root:                    VPath,
    entries:                 Vec<FileEntry>,
    selected:                usize,
    show_hidden:             bool,
    tree_scroll:             usize,  // first visible entry index
    tree_search:             String,
    tree_search_cursor:      usize,
    tree_search_sel_anchor:  Option<usize>,
    tree_search_focused:     bool,
    tree_search_fuzzy:       bool,
    tree_search_entries:     Vec<std::path::PathBuf>,
    tree_search_results:     Vec<std::path::PathBuf>,
    tree_search_sel:         usize,
    last_refresh:            Instant,
    pending_remote_listings: HashSet<PathBuf>,
    /// A background walk for the tree filter is running / another one is wanted.
    tree_search_walking:     bool,
    tree_search_rewalk:      bool,
}

impl FileExplorer {
    fn new(root: VPath) -> Self {
        // Remote directory listing is async (Phase 3); start empty for remote roots.
        let entries = root.as_local_path()
            .map(|p| load_dir_entries(p, 0, false))
            .unwrap_or_default();
        FileExplorer {
            root, entries, selected: 0, show_hidden: false, tree_scroll: 0,
            tree_search: String::new(), tree_search_cursor: 0, tree_search_sel_anchor: None,
            tree_search_focused: false, tree_search_fuzzy: true,
            tree_search_entries: vec![], tree_search_results: vec![], tree_search_sel: 0,
            last_refresh: Instant::now(),
            pending_remote_listings: HashSet::new(),
            tree_search_walking: false,
            tree_search_rewalk: false,
        }
    }

    fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        if let Some(local) = self.root.as_local_path() {
            self.entries = load_dir_entries(local, 0, self.show_hidden);
        }
        self.selected = 0;
    }

    fn toggle(&mut self, idx: usize) -> Option<PathBuf> {
        if !self.entries[idx].is_dir { return None; }
        if self.entries[idx].expanded {
            self.entries[idx].expanded = false;
            let depth = self.entries[idx].depth;
            let mut end = idx + 1;
            while end < self.entries.len() && self.entries[end].depth > depth { end += 1; }
            self.entries.drain(idx + 1..end);
        } else {
            self.entries[idx].expanded = true;
            let path = self.entries[idx].path.clone();
            let depth = self.entries[idx].depth + 1;
            if self.root.as_local_path().is_some() {
                let children = load_dir_entries(&path, depth, self.show_hidden);
                for (i, child) in children.into_iter().enumerate() {
                    self.entries.insert(idx + 1 + i, child);
                }
            } else {
                self.selected = self.selected.min(self.entries.len().saturating_sub(1));
                return Some(path);
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        None
    }

    fn refresh_local(&mut self) -> bool {
        let Some(root) = self.root.as_local_path().map(|path| path.to_path_buf()) else {
            return false;
        };
        let expanded: HashSet<PathBuf> = self.entries.iter()
            .filter(|entry| entry.is_dir && entry.expanded)
            .map(|entry| entry.path.clone())
            .collect();
        let selected_path = self.entries.get(self.selected).map(|entry| entry.path.clone());
        let mut refreshed = Vec::new();
        append_refreshed_entries(
            &root,
            0,
            self.show_hidden,
            &expanded,
            &mut refreshed,
        );
        if refreshed == self.entries {
            return false;
        }
        self.entries = refreshed;
        self.selected = selected_path
            .and_then(|path| self.entries.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| self.selected.min(self.entries.len().saturating_sub(1)));
        self.tree_scroll = self.tree_scroll.min(self.entries.len().saturating_sub(1));
        // The file set changed: the tree filter's file list needs a fresh walk. It
        // runs in the background (see `spawn_tree_search_walk`), never here on the
        // UI thread.
        if !self.tree_search.is_empty() || !self.tree_search_entries.is_empty() {
            self.tree_search_rewalk = true;
        }
        true
    }
}

fn append_refreshed_entries(
    dir: &std::path::Path,
    depth: usize,
    show_hidden: bool,
    expanded: &HashSet<PathBuf>,
    out: &mut Vec<FileEntry>,
) {
    for mut entry in load_dir_entries(dir, depth, show_hidden) {
        let should_expand = entry.is_dir && expanded.contains(&entry.path);
        entry.expanded = should_expand;
        let child_dir = entry.path.clone();
        out.push(entry);
        if should_expand {
            append_refreshed_entries(
                &child_dir,
                depth + 1,
                show_hidden,
                expanded,
                out,
            );
        }
    }
}

fn load_dir_entries(dir: &std::path::Path, depth: usize, show_hidden: bool) -> Vec<FileEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut entries: Vec<FileEntry> = rd
        .filter_map(|e| e.ok())
        .filter(|e| show_hidden || !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| {
            let path = e.path();
            let is_dir = path.is_dir();
            let name = e.file_name().to_string_lossy().into_owned();
            FileEntry { name, path, is_dir, expanded: false, depth }
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

fn remote_uri(vpath: &VPath) -> Option<String> {
    match vpath {
        VPath::Remote { host, path } => Some(format!("ssh://{}:{}", host.display(), path.display())),
        VPath::Local(_) => None,
    }
}

fn parse_remote_directory_query(query: &str) -> Result<(VPath, String), String> {
    let query = query.trim();
    if !query.starts_with("ssh://") {
        return Err("Remote directory must start with ssh://".to_owned());
    }
    if query == "ssh://" {
        return Err("Enter a remote URI like ssh://host:~/devel".to_owned());
    }

    let vpath = VPath::parse(query);
    if matches!(&vpath, VPath::Remote { .. }) {
        let uri = remote_uri(&vpath).unwrap_or_else(|| query.to_owned());
        Ok((vpath, uri))
    } else {
        Err("Remote URI must look like ssh://host:~/devel".to_owned())
    }
}

fn open_remote_directory(s: &mut State, vpath: VPath, uri: String) {
    let h = match &vpath {
        VPath::Remote { host, .. } => host.clone(),
        VPath::Local(_) => return,
    };
    s.explorer = Some(FileExplorer::new(vpath));
    s.left_view = LeftView::FileTree;
    s.explorer_w = ((200.0 * s.font_size / FONT_PX).round() as i32).clamp(80, 600);
    eprintln!("open remote directory: {uri}");
    if !s.settings.recent_remote_hosts.contains(&uri) {
        s.settings.recent_remote_hosts.insert(0, uri);
        s.settings.recent_remote_hosts.truncate(20);
        s.settings.save();
    }
    s.status_msg = Some(format!("Connecting to {}…", h.display()));
    ssh::ensure_control_master(h, s.proxy.clone());
}

fn remote_listing_to_file_entries(
    parent: &std::path::Path,
    entries: Vec<(String, bool)>,
    depth: usize,
    show_hidden: bool,
) -> Vec<FileEntry> {
    fn remote_child_path(parent: &std::path::Path, name: &str) -> PathBuf {
        let parent = parent.to_string_lossy();
        let sep = if parent == "/" || parent.ends_with('/') { "" } else { "/" };
        PathBuf::from(format!("{parent}{sep}{name}"))
    }

    let mut out: Vec<FileEntry> = entries.into_iter()
        .filter(|(name, _)| show_hidden || !name.starts_with('.'))
        .map(|(name, is_dir)| FileEntry {
            path: remote_child_path(parent, &name),
            name,
            is_dir,
            expanded: false,
            depth,
        })
        .collect();
    out.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

fn merge_remote_children(
    old: &[FileEntry],
    new_children: Vec<FileEntry>,
    child_depth: usize,
) -> Vec<FileEntry> {
    let mut merged = Vec::new();
    for mut child in new_children {
        if let Some(old_idx) = old.iter().position(|entry| {
            entry.depth == child_depth && entry.path == child.path
        }) {
            child.expanded = child.is_dir && old[old_idx].expanded;
            merged.push(child.clone());
            if child.expanded {
                for descendant in old.iter().skip(old_idx + 1)
                    .take_while(|entry| entry.depth > child_depth)
                {
                    merged.push(descendant.clone());
                }
            }
        } else {
            merged.push(child);
        }
    }
    merged
}

fn apply_remote_dir_listing(
    explorer: &mut FileExplorer,
    listing_path: &std::path::Path,
    entries: Vec<(String, bool)>,
) {
    let selected_path = explorer.entries.get(explorer.selected)
        .map(|entry| entry.path.clone());
    if explorer.root.as_path() == listing_path {
        let new_children = remote_listing_to_file_entries(
            listing_path,
            entries,
            0,
            explorer.show_hidden,
        );
        let old = std::mem::take(&mut explorer.entries);
        explorer.entries = merge_remote_children(&old, new_children, 0);
    } else if let Some(parent_idx) = explorer.entries.iter()
        .position(|entry| entry.path == listing_path)
    {
        let parent_depth = explorer.entries[parent_idx].depth;
        let child_depth = parent_depth + 1;
        let mut end = parent_idx + 1;
        while end < explorer.entries.len() && explorer.entries[end].depth > parent_depth {
            end += 1;
        }
        if explorer.entries[parent_idx].expanded {
            let old_children = explorer.entries[parent_idx + 1..end].to_vec();
            let new_children = remote_listing_to_file_entries(
                listing_path,
                entries,
                child_depth,
                explorer.show_hidden,
            );
            let merged = merge_remote_children(&old_children, new_children, child_depth);
            explorer.entries.splice(parent_idx + 1..end, merged);
        }
    }
    explorer.selected = selected_path
        .and_then(|path| explorer.entries.iter().position(|entry| entry.path == path))
        .unwrap_or_else(|| explorer.selected.min(explorer.entries.len().saturating_sub(1)));
    explorer.tree_scroll = explorer.tree_scroll.min(explorer.entries.len().saturating_sub(1));
}

fn explorer_entry_vpath(ex: &FileExplorer, path: PathBuf) -> VPath {
    match &ex.root {
        VPath::Remote { host, .. } => VPath::Remote { host: host.clone(), path },
        VPath::Local(_) => VPath::Local(path),
    }
}

// ── Pane layout types ─────────────────────────────────────────────────────────
const DRAG_ZONE: u32 = 0x2A3F6F;

#[derive(Clone, Copy)]
struct Rect { x: i32, y: i32, w: i32, h: i32 }

#[derive(Clone, Copy, PartialEq)]
enum Axis { H, V }

#[derive(Clone, Copy, PartialEq, Debug)]
enum DropZone { Center, Left, Right, Top, Bottom }

enum PaneTree {
    Leaf(usize),
    Split { axis: Axis, ratio: f32, a: Box<PaneTree>, b: Box<PaneTree> },
}

#[derive(Clone, PartialEq)]
enum PaneKind { Editor, Terminal, MarkdownPreview }

struct Pane {
    id:       usize,
    kind:     PaneKind,
    tabs:     Vec<Tab>,     // editor tabs (Editor panes)
    term_ids: Vec<usize>,   // terminal session IDs (Terminal panes)
    active:   usize,        // index into tabs or term_ids
    find:     FindBar,
}

impl Pane {
    fn new(id: usize, buf_id: usize) -> Self {
        Pane { id, kind: PaneKind::Editor, tabs: vec![Tab::untitled(buf_id)],
               term_ids: vec![], active: 0, find: FindBar::new() }
    }
    fn tab(&self)     -> &Tab     { &self.tabs[self.active] }
    fn tab_mut(&mut self) -> &mut Tab { &mut self.tabs[self.active] }
    fn display_name(&self) -> &str { self.tab().display_name() }
}

struct DragState {
    source_pane: usize,
    source_tab:  usize,
    cur_x:     f32,
    cur_y:     f32,
    over_pane: Option<usize>,
    zone:      Option<DropZone>,
}

struct ResizeDrag {
    path: Vec<bool>,   // path through tree to the Split node (false=left/top, true=right/bottom)
    axis: Axis,
    rect: Rect,        // bounding rect of that Split node
}

const BORDER_HIT: i32 = 4;  // px on each side of the 1px divider that triggers resize

/// Walk the tree to find if (mx, my) is within BORDER_HIT of any split divider.
/// Returns the path, axis, and bounding rect of the matching Split node.
fn find_border_at(tree: &PaneTree, rect: Rect, mx: i32, my: i32) -> Option<(Vec<bool>, Axis, Rect)> {
    match tree {
        PaneTree::Leaf(_) => None,
        PaneTree::Split { axis, ratio, a, b } => {
            let (ra, rb) = split_rect(rect, *axis, *ratio);
            let on_border = match axis {
                Axis::H => {
                    let bx = ra.x + ra.w;   // x of the 1px gap
                    mx >= bx - BORDER_HIT && mx <= bx + BORDER_HIT
                        && my >= rect.y && my < rect.y + rect.h
                }
                Axis::V => {
                    let by = ra.y + ra.h;   // y of the 1px gap
                    my >= by - BORDER_HIT && my <= by + BORDER_HIT
                        && mx >= rect.x && mx < rect.x + rect.w
                }
            };
            if on_border { return Some((vec![], *axis, rect)); }
            if let Some((mut path, ax, r)) = find_border_at(a, ra, mx, my) {
                path.insert(0, false);
                return Some((path, ax, r));
            }
            if let Some((mut path, ax, r)) = find_border_at(b, rb, mx, my) {
                path.insert(0, true);
                return Some((path, ax, r));
            }
            None
        }
    }
}

/// Update the ratio of the Split node reached by following `path` from the root.
fn update_ratio(tree: &mut PaneTree, path: &[bool], new_ratio: f32) {
    if let PaneTree::Split { ratio, a, b, .. } = tree {
        if path.is_empty() {
            *ratio = new_ratio.clamp(0.05, 0.95);
        } else {
            let child = if path[0] { b.as_mut() } else { a.as_mut() };
            update_ratio(child, &path[1..], new_ratio);
        }
    }
}

fn split_rect(rect: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    match axis {
        Axis::H => {
            let s = (rect.w as f32 * ratio) as i32;
            let a = Rect { x: rect.x,         y: rect.y, w: s,                h: rect.h };
            let b = Rect { x: rect.x + s + 1, y: rect.y, w: rect.w - s - 1,  h: rect.h };
            (a, b)
        }
        Axis::V => {
            let s = (rect.h as f32 * ratio) as i32;
            let a = Rect { x: rect.x, y: rect.y,         w: rect.w, h: s };
            let b = Rect { x: rect.x, y: rect.y + s + 1, w: rect.w, h: rect.h - s - 1 };
            (a, b)
        }
    }
}

fn layout_tree(tree: &PaneTree, rect: Rect) -> Vec<(usize, Rect)> {
    match tree {
        PaneTree::Leaf(id) => vec![(*id, rect)],
        PaneTree::Split { axis, ratio, a, b } => {
            let (ra, rb) = split_rect(rect, *axis, *ratio);
            let mut v = layout_tree(a, ra);
            v.extend(layout_tree(b, rb));
            v
        }
    }
}

fn pane_at_pos(tree: &PaneTree, mx: i32, my: i32, rect: Rect) -> Option<usize> {
    match tree {
        PaneTree::Leaf(id) => {
            if mx >= rect.x && mx < rect.x + rect.w && my >= rect.y && my < rect.y + rect.h {
                Some(*id)
            } else { None }
        }
        PaneTree::Split { axis, ratio, a, b } => {
            let (ra, rb) = split_rect(rect, *axis, *ratio);
            pane_at_pos(a, mx, my, ra).or_else(|| pane_at_pos(b, mx, my, rb))
        }
    }
}

fn drop_zone(mx: i32, my: i32, pane_rect: Rect, tab_h: i32) -> DropZone {
    let ey = pane_rect.y + tab_h;
    let eh = pane_rect.h - tab_h;
    if eh <= 0 { return DropZone::Center; }
    let rx = (mx - pane_rect.x) as f32 / pane_rect.w as f32;
    let ry = (my - ey) as f32 / eh as f32;
    if rx < 0.25      { DropZone::Left }
    else if rx > 0.75 { DropZone::Right }
    else if ry < 0.25 { DropZone::Top }
    else if ry > 0.75 { DropZone::Bottom }
    else              { DropZone::Center }
}

fn insert_pane(tree: PaneTree, target_id: usize, new_id: usize, zone: DropZone) -> PaneTree {
    match tree {
        PaneTree::Leaf(id) if id == target_id => {
            let (axis, new_first) = match zone {
                DropZone::Left   => (Axis::H, true),
                DropZone::Right  => (Axis::H, false),
                DropZone::Top    => (Axis::V, true),
                DropZone::Bottom => (Axis::V, false),
                DropZone::Center => return PaneTree::Leaf(id),
            };
            let (a, b): (Box<PaneTree>, Box<PaneTree>) = if new_first {
                (Box::new(PaneTree::Leaf(new_id)), Box::new(PaneTree::Leaf(id)))
            } else {
                (Box::new(PaneTree::Leaf(id)), Box::new(PaneTree::Leaf(new_id)))
            };
            PaneTree::Split { axis, ratio: 0.5, a, b }
        }
        PaneTree::Leaf(id) => PaneTree::Leaf(id),
        PaneTree::Split { axis, ratio, a, b } => PaneTree::Split {
            axis, ratio,
            a: Box::new(insert_pane(*a, target_id, new_id, zone)),
            b: Box::new(insert_pane(*b, target_id, new_id, zone)),
        }
    }
}

fn remove_pane_from_tree(tree: PaneTree, target_id: usize) -> Option<PaneTree> {
    match tree {
        PaneTree::Leaf(id) if id == target_id => None,
        PaneTree::Leaf(id) => Some(PaneTree::Leaf(id)),
        PaneTree::Split { axis, ratio, a, b } => {
            match (remove_pane_from_tree(*a, target_id), remove_pane_from_tree(*b, target_id)) {
                (None, Some(t)) | (Some(t), None) => Some(t),
                (Some(a), Some(b)) => Some(PaneTree::Split { axis, ratio, a: Box::new(a), b: Box::new(b) }),
                (None, None) => None,
            }
        }
    }
}

fn pane_tree_contains(tree: &PaneTree, target_id: usize) -> bool {
    match tree {
        PaneTree::Leaf(id) => *id == target_id,
        PaneTree::Split { a, b, .. } => pane_tree_contains(a, target_id) || pane_tree_contains(b, target_id),
    }
}

fn first_visible_pane_id(s: &State) -> Option<usize> {
    layout_tree(&s.pane_tree, s.pane_area())
        .into_iter()
        .map(|(id, _)| id)
        .find(|id| s.panes.contains_key(id))
}

/// Return an Editor pane suitable for hosting a new tab: the active pane if it's
/// an editor, otherwise the first editor pane in the layout. If no editor pane
/// exists — e.g. every editor tab was closed, leaving only a terminal pane (or
/// nothing) — create one, splitting beside an existing pane or as the sole pane.
/// `active_pane` is pointed at the result. The returned pane always has at least
/// one tab, preserving the "an editor pane is never tab-empty" invariant.
fn ensure_editor_pane(s: &mut State) -> usize {
    if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Editor) {
        return s.active_pane;
    }
    let area = s.pane_area();
    if let Some(id) = layout_tree(&s.pane_tree, area).into_iter()
        .find(|(id, _)| s.panes.get(id).map_or(false, |p| p.kind == PaneKind::Editor))
        .map(|(id, _)| id)
    {
        s.active_pane = id;
        return id;
    }
    let pane_id = s.next_pane_id;
    s.next_pane_id += 1;
    let buf_id = s.next_buf_id;
    s.next_buf_id += 1;
    s.panes.insert(pane_id, Pane::new(pane_id, buf_id));
    let target = first_visible_pane_id(s);
    let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
    s.pane_tree = match target {
        Some(root) => insert_pane(old_tree, root, pane_id, DropZone::Right),
        None        => PaneTree::Leaf(pane_id),
    };
    s.active_pane = pane_id;
    pane_id
}

fn remove_pane_from_layout(s: &mut State, pane_id: usize) -> bool {
    if !pane_tree_contains(&s.pane_tree, pane_id) {
        if !pane_tree_contains(&s.pane_tree, s.active_pane) || !s.panes.contains_key(&s.active_pane) {
            if let Some(id) = first_visible_pane_id(s) {
                s.active_pane = id;
            } else {
                s.pane_tree = PaneTree::Leaf(0);
                s.active_pane = 0;
                return true;
            }
        }
        return false;
    }

    let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
    if let Some(new_tree) = remove_pane_from_tree(old_tree, pane_id) {
        s.pane_tree = new_tree;
        if let Some(id) = first_visible_pane_id(s) {
            if !pane_tree_contains(&s.pane_tree, s.active_pane) || !s.panes.contains_key(&s.active_pane) {
                s.active_pane = id;
            }
            false
        } else {
            s.pane_tree = PaneTree::Leaf(0);
            s.active_pane = 0;
            true
        }
    } else {
        s.pane_tree = PaneTree::Leaf(0);
        s.active_pane = 0;
        true
    }
}

fn notify_lsp_tab_closed(s: &mut State, path: Option<VPath>, lang: Lang) {
    let Some(path) = path else { return };
    if let Some(srv) = s.lsp.server_for_lang_host_mut(lang, path.ssh_host()) {
        lsp::notify_did_close(srv, &path);
    }
}

fn close_editor_tab(s: &mut State, pane_id: usize, tab_idx: usize) -> bool {
    let (closed_path, closed_lang) = {
        let Some(pane) = s.panes.get_mut(&pane_id) else { return false };
        if pane.kind != PaneKind::Editor || tab_idx >= pane.tabs.len() {
            return false;
        }
        let closed = pane.tabs.remove(tab_idx);
        let closed_lang = closed.lang();
        if pane.active >= pane.tabs.len() && !pane.tabs.is_empty() {
            pane.active = pane.tabs.len() - 1;
        }
        (closed.path, closed_lang)
    };

    // Keep the document open on the server while another tab still shows it.
    let still_open = closed_path.as_ref().map_or(false, |p| find_tab_for_path(s, p).is_some());
    if !still_open {
        if let Some(p) = &closed_path { s.diagnostics.remove(p); }
        notify_lsp_tab_closed(s, closed_path, closed_lang);
    }

    if s.panes.get(&pane_id).map_or(false, |p| p.tabs.is_empty()) {
        s.md_panes.remove(&pane_id);
        s.panes.remove(&pane_id);
        remove_pane_from_layout(s, pane_id)
    } else {
        false
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum UnsavedChoice { Save, Discard, Cancel }

/// Ask what to do with unsaved changes: a native dialog on macOS and Windows, and
/// zenity / kdialog on Linux. Without either of those, the first attempt is refused
/// with a status-bar explanation and repeating the same action within 5 s discards.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(unused_variables))] // `s`/`key` drive the Linux fallback
fn ask_unsaved(s: &mut State, key: &str, message: &str, can_save: bool) -> UnsavedChoice {
    #[cfg(target_os = "macos")]
    {
        let buttons = if can_save { r#"{"Cancel", "Don't Save", "Save"}"# } else { r#"{"Cancel", "Don't Save"}"# };
        let default = if can_save { "Save" } else { "Cancel" };
        let script = format!(r#"
on run argv
    set r to display dialog (item 1 of argv) buttons {buttons} default button "{default}" cancel button "Cancel" with icon caution
    return button returned of r
end run
"#);
        let out = std::process::Command::new("osascript")
            .args(["-e", &script, "--", message])
            .output();
        let Ok(out) = out else { return UnsavedChoice::Cancel };
        if !out.status.success() { return UnsavedChoice::Cancel; }
        match String::from_utf8_lossy(&out.stdout).trim() {
            "Save" => UnsavedChoice::Save,
            "Don't Save" => UnsavedChoice::Discard,
            _ => UnsavedChoice::Cancel,
        }
    }
    #[cfg(windows)]
    {
        if can_save {
            match os::dialogs::yes_no_cancel(&format!("{message}\n\nYes: Save    No: Don't Save"), "local-text") {
                Some(true) => UnsavedChoice::Save,
                Some(false) => UnsavedChoice::Discard,
                None => UnsavedChoice::Cancel,
            }
        } else if os::dialogs::ok_cancel(&format!("{message}\n\nOK: Don't Save"), "local-text") {
            UnsavedChoice::Discard
        } else {
            UnsavedChoice::Cancel
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(choice) = linux_unsaved_dialog(message, can_save) {
            return choice;
        }
        let now = Instant::now();
        if let Some((armed, at)) = &s.discard_armed {
            if armed == key && now.duration_since(*at) < Duration::from_secs(5) {
                s.discard_armed = None;
                return UnsavedChoice::Discard;
            }
        }
        s.discard_armed = Some((key.to_owned(), now));
        s.status_msg = Some(format!("{message} Save first (Ctrl+S), or repeat within 5 s to discard."));
        UnsavedChoice::Cancel
    }
}

/// zenity / kdialog Save / Don't Save / Cancel question. None when neither is
/// installed (the caller then falls back to the repeat-to-discard prompt).
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_unsaved_dialog(message: &str, can_save: bool) -> Option<UnsavedChoice> {
    let run = |bin: &str, args: &[&str]| {
        os::command(bin).args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output().ok()
    };
    // zenity renders --text as Pango markup: a file name with & or < must be escaped.
    let text = format!("--text={}", message.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"));
    let mut zenity: Vec<&str> = vec!["--question", "--title=local-text", "--no-wrap", &text, "--cancel-label=Cancel"];
    if can_save {
        zenity.extend(["--ok-label=Save", "--extra-button=Don't Save"]);
    } else {
        zenity.push("--ok-label=Don't Save");
    }
    if let Some(out) = run("zenity", &zenity) {
        // Save/OK → exit 0; the extra button prints its label and exits 1; Cancel or
        // closing the window exits 1 with no output.
        return Some(if out.status.success() {
            if can_save { UnsavedChoice::Save } else { UnsavedChoice::Discard }
        } else if String::from_utf8_lossy(&out.stdout).trim() == "Don't Save" {
            UnsavedChoice::Discard
        } else {
            UnsavedChoice::Cancel
        });
    }
    let out = if can_save {
        run("kdialog", &["--title", "local-text", "--warningyesnocancel", message,
            "--yes-label", "Save", "--no-label", "Don't Save"])
    } else {
        run("kdialog", &["--title", "local-text", "--warningcontinuecancel", message,
            "--continue-label", "Don't Save"])
    }?;
    // kdialog: 0 = yes/continue, 1 = no, 2 = cancel.
    Some(match (out.status.code(), can_save) {
        (Some(0), true) => UnsavedChoice::Save,
        (Some(0), false) | (Some(1), true) => UnsavedChoice::Discard,
        _ => UnsavedChoice::Cancel,
    })
}

/// True if another tab shares this tab's buffer (closing one view loses nothing).
fn has_buffer_sibling(s: &State, pane_id: usize, tab_idx: usize) -> bool {
    let Some(buf_id) = s.panes.get(&pane_id).and_then(|p| p.tabs.get(tab_idx)).map(|t| t.buf_id) else { return false };
    s.panes.iter().any(|(&pid, p)| p.tabs.iter().enumerate().any(|(ti, t)| {
        t.buf_id == buf_id && t.kind == TabKind::Editor && (pid != pane_id || ti != tab_idx)
    }))
}

/// Close an editor tab, first asking about unsaved changes. Returns None if the
/// user cancelled, otherwise `Some(app_should_exit)` like `close_editor_tab`.
fn request_close_editor_tab(s: &mut State, pane_id: usize, tab_idx: usize) -> Option<bool> {
    let tab = s.panes.get(&pane_id).and_then(|p| p.tabs.get(tab_idx))?;
    // A guest's shared tab is never "unsaved": the host owns the file.
    let needs_prompt = tab.kind == TabKind::Editor && tab.dirty && tab.collab_path.is_none()
        && !has_buffer_sibling(s, pane_id, tab_idx);
    if needs_prompt {
        let name = tab.display_name().to_owned();
        let key = format!("close:{}", tab.buf_id);
        match ask_unsaved(s, &key, &format!("Do you want to save the changes you made to {name}?"), true) {
            UnsavedChoice::Save => {
                if !save_tab(s, pane_id, tab_idx) { return None; }
            }
            UnsavedChoice::Discard => {}
            UnsavedChoice::Cancel => return None,
        }
    }
    collab_tab_closing(s, pane_id, tab_idx);
    Some(close_editor_tab(s, pane_id, tab_idx))
}

/// Dirty editor tabs, one per buffer (split views share a buffer).
fn dirty_buffers(s: &State) -> Vec<(usize, usize)> {
    let mut seen = HashSet::new();
    let mut ids: Vec<usize> = s.panes.keys().copied().collect();
    ids.sort_unstable();
    let mut out = Vec::new();
    for pid in ids {
        for (ti, t) in s.panes[&pid].tabs.iter().enumerate() {
            if t.kind == TabKind::Editor && t.dirty && t.collab_path.is_none() && seen.insert(t.buf_id) {
                out.push((pid, ti));
            }
        }
    }
    out
}

/// Window close: ask about unsaved buffers. Returns true if it's OK to quit.
/// Saving here is synchronous (remote files too), so nothing is still being
/// written when the process exits.
fn request_quit(s: &mut State) -> bool {
    let dirty = dirty_buffers(s);
    if dirty.is_empty() { return true; }
    let names: Vec<String> = dirty.iter().map(|&(pid, ti)| s.panes[&pid].tabs[ti].display_name().to_owned()).collect();
    let msg = if names.len() == 1 {
        format!("Do you want to save the changes you made to {} before quitting?", names[0])
    } else {
        format!("{} files have unsaved changes ({}). Save them before quitting?", names.len(), names.join(", "))
    };
    match ask_unsaved(s, "quit", &msg, true) {
        UnsavedChoice::Cancel => false,
        UnsavedChoice::Discard => true,
        UnsavedChoice::Save => {
            for (pid, ti) in dirty {
                let tab = &s.panes[&pid].tabs[ti];
                let ok = match tab.path.clone() {
                    Some(VPath::Remote { host, path }) => {
                        let content: String = tab.text.chunks().collect();
                        match ssh::ssh_write_file_blocking(&host, &path, &content) {
                            Ok(()) => { s.panes.get_mut(&pid).unwrap().tabs[ti].dirty = false; true }
                            Err(e) => { s.status_msg = Some(format!("Save failed ({}): {e}", path.display())); false }
                        }
                    }
                    _ => save_tab(s, pid, ti),
                };
                if !ok { return false; } // keep the window open so nothing is lost
            }
            true
        }
    }
}

/// Last-resort safety net: copy every unsaved buffer to
/// `<data dir>/recovery/<time>-<pid>/` (see `os::data_dir`; used when the app exits
/// without our prompt — e.g. Cmd+Q from the menu — or after a crash).
fn write_recovery_files(s: &State) -> Option<PathBuf> {
    let dirty = dirty_buffers(s);
    if dirty.is_empty() { return None; }
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let dir = recovery_base_dir()?.join(format!("{ts}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let mut index = String::new();
    for (n, (pid, ti)) in dirty.into_iter().enumerate() {
        let tab = &s.panes[&pid].tabs[ti];
        // Strip characters that aren't allowed in file names on any platform.
        let name = format!("{n}-{}", tab.display_name().replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_"));
        let content: String = tab.text.chunks().collect();
        if std::fs::write(dir.join(&name), content).is_ok() {
            let original = tab.path.as_ref().map(|p| p.to_string()).unwrap_or_else(|| "(untitled)".to_owned());
            index.push_str(&format!("{name}\t{original}\n"));
        }
    }
    let _ = std::fs::write(dir.join("index.txt"), index);
    eprintln!("local-text: unsaved changes were saved to {}", dir.display());
    Some(dir)
}

fn recovery_base_dir() -> Option<PathBuf> {
    Some(os::data_dir()?.join("recovery"))
}

fn close_terminal_tab(s: &mut State, pane_id: usize, tab_idx: usize) -> bool {
    let term_id = {
        let Some(pane) = s.panes.get_mut(&pane_id) else { return false };
        if pane.kind != PaneKind::Terminal || tab_idx >= pane.term_ids.len() {
            return false;
        }
        let term_id = pane.term_ids.remove(tab_idx);
        if pane.active >= pane.term_ids.len() && !pane.term_ids.is_empty() {
            pane.active = pane.term_ids.len() - 1;
        }
        term_id
    };
    s.term_panes.remove(&term_id);

    if s.panes.get(&pane_id).map_or(false, |p| p.term_ids.is_empty()) {
        s.panes.remove(&pane_id);
        remove_pane_from_layout(s, pane_id)
    } else {
        false
    }
}

fn show_lsp_output_pane(s: &mut State, pane_id: usize) {
    if !s.panes.contains_key(&pane_id) {
        return;
    }
    if pane_tree_contains(&s.pane_tree, pane_id) {
        s.active_pane = pane_id;
        return;
    }

    let target = if pane_tree_contains(&s.pane_tree, s.active_pane) && s.panes.contains_key(&s.active_pane) {
        Some(s.active_pane)
    } else {
        first_visible_pane_id(s)
    };

    if let Some(target_id) = target {
        let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
        s.pane_tree = insert_pane(old_tree, target_id, pane_id, DropZone::Bottom);
    } else {
        s.pane_tree = PaneTree::Leaf(pane_id);
    }
    s.active_pane = pane_id;
}

fn hide_lsp_output_pane(s: &mut State, pane_id: usize) -> bool {
    remove_pane_from_layout(s, pane_id)
}

fn jump_to_path_line_col(s: &mut State, path: VPath, line: usize, col: usize) -> bool {
    open_or_reuse_tab(s, path.clone());

    let Some((pane_id, tab_idx)) = find_tab_for_path(s, &path) else { return false };

    s.active_pane = pane_id;
    if let Some(pane) = s.panes.get_mut(&pane_id) {
        pane.active = tab_idx;
    }

    let jump = {
        let Some(tab) = s.panes.get(&pane_id).and_then(|pane| pane.tabs.get(tab_idx)) else {
            return false;
        };
        if tab.remote_load_token.is_some() {
            s.status_msg = Some(format!(
                "Loading {} before jumping to line {}",
                path.display_short(),
                line.saturating_add(1)
            ));
            return false;
        }
        let target_line = line.min(State::last_line(&tab.text));
        let target_col = col.min(State::line_len(&tab.text, target_line));
        tab.text.line_to_char(target_line) + target_col
    };

    if let Some(pane) = s.panes.get_mut(&pane_id) {
        if let Some(tab) = pane.tabs.get_mut(tab_idx) {
            tab.cursors = vec![Cursor::new(jump)];
        }
    }
    s.ensure_visible();
    true
}

fn perform_drop(s: &mut State, drag: DragState) {
    let (src_pid, src_tidx) = (drag.source_pane, drag.source_tab);
    let Some(dst_pid) = drag.over_pane else { return };
    let Some(zone)    = drag.zone      else { return };

    let src_kind = s.panes.get(&src_pid).map(|p| p.kind.clone()).unwrap_or(PaneKind::Editor);
    let dst_kind = s.panes.get(&dst_pid).map(|p| p.kind.clone()).unwrap_or(PaneKind::Editor);

    match src_kind {
        PaneKind::Editor => {
            if src_pid == dst_pid && s.panes[&src_pid].tabs.len() == 1 { return; }
            if src_pid == dst_pid && zone == DropZone::Center { return; }
            // Can't center-drop an editor tab onto a non-editor pane
            if zone == DropZone::Center && dst_kind != PaneKind::Editor { return; }

            if src_tidx >= s.panes[&src_pid].tabs.len() { return; }
            let tab = s.panes.get_mut(&src_pid).unwrap().tabs.remove(src_tidx);
            {
                let sp = s.panes.get_mut(&src_pid).unwrap();
                if sp.active >= sp.tabs.len() && !sp.tabs.is_empty() { sp.active = sp.tabs.len() - 1; }
            }
            if zone == DropZone::Center {
                let dp = s.panes.get_mut(&dst_pid).unwrap();
                dp.tabs.push(tab);
                dp.active = dp.tabs.len() - 1;
                s.active_pane = dst_pid;
            } else {
                let new_id = s.next_pane_id; s.next_pane_id += 1;
                let mut new_pane = Pane::new(new_id, 0);
                new_pane.tabs = vec![tab];
                new_pane.active = 0;
                s.panes.insert(new_id, new_pane);
                let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                s.pane_tree = insert_pane(old_tree, dst_pid, new_id, zone);
                s.active_pane = new_id;
            }
            if s.panes.get(&src_pid).map_or(false, |p| p.tabs.is_empty()) && src_pid != dst_pid {
                s.md_panes.remove(&src_pid);
                s.panes.remove(&src_pid);
                let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                if let Some(t) = remove_pane_from_tree(old_tree, src_pid) { s.pane_tree = t; }
                if !s.panes.contains_key(&s.active_pane) {
                    s.active_pane = s.panes.keys().copied().next().unwrap_or(0);
                }
            }
        }
        PaneKind::Terminal => {
            if src_tidx >= s.panes[&src_pid].term_ids.len() { return; }
            if src_pid == dst_pid && s.panes[&src_pid].term_ids.len() == 1 { return; }
            if src_pid == dst_pid && zone == DropZone::Center { return; }
            // Can't center-drop a terminal onto a non-terminal pane
            if zone == DropZone::Center && dst_kind != PaneKind::Terminal { return; }

            let term_id = s.panes.get_mut(&src_pid).unwrap().term_ids.remove(src_tidx);
            {
                let sp = s.panes.get_mut(&src_pid).unwrap();
                if sp.active >= sp.term_ids.len() && !sp.term_ids.is_empty() {
                    sp.active = sp.term_ids.len() - 1;
                }
            }
            if zone == DropZone::Center {
                let dp = s.panes.get_mut(&dst_pid).unwrap();
                dp.term_ids.push(term_id);
                dp.active = dp.term_ids.len() - 1;
                s.active_pane = dst_pid;
            } else {
                let new_id = s.next_pane_id; s.next_pane_id += 1;
                let new_pane = Pane { id: new_id, kind: PaneKind::Terminal, tabs: vec![],
                                      term_ids: vec![term_id], active: 0, find: FindBar::new() };
                s.panes.insert(new_id, new_pane);
                let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                s.pane_tree = insert_pane(old_tree, dst_pid, new_id, zone);
                s.active_pane = new_id;
            }
            if s.panes.get(&src_pid).map_or(false, |p| p.term_ids.is_empty()) && src_pid != dst_pid {
                s.panes.remove(&src_pid);
                let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                if let Some(t) = remove_pane_from_tree(old_tree, src_pid) { s.pane_tree = t; }
                if !s.panes.contains_key(&s.active_pane) {
                    s.active_pane = s.panes.keys().copied().next().unwrap_or(0);
                }
            }
        }
        PaneKind::MarkdownPreview => {
            // Single-unit pane: can't merge into another pane, only re-split
            if src_pid == dst_pid { return; }
            if zone == DropZone::Center { return; }
            // Remove from old tree location
            let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
            let Some(trimmed) = remove_pane_from_tree(old_tree, src_pid) else { return; };
            s.pane_tree = trimmed;
            if !pane_tree_contains(&s.pane_tree, s.active_pane) || !s.panes.contains_key(&s.active_pane) {
                if let Some(id) = first_visible_pane_id(s) { s.active_pane = id; }
            }
            // Insert at new location
            let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
            s.pane_tree = insert_pane(old_tree, dst_pid, src_pid, zone);
            s.active_pane = src_pid;
        }
    }
}

fn resize_terminal_panes(s: &mut State) {
    let area = s.pane_area();
    let tab_h = s.tab_h();
    let lh    = s.glyphs.lh;
    let cw    = s.glyphs.cw;
    let layout = layout_tree(&s.pane_tree, area);
    for (pid, rect) in layout {
        let is_term_like = s.panes.get(&pid).map_or(false, |p| p.kind == PaneKind::Terminal);
        if is_term_like {
            let cols = (rect.w / cw).max(1) as usize;
            let rows = if rect.h > tab_h { ((rect.h - tab_h) / lh).max(1) as usize } else { 1 };
            let tids: Vec<usize> = s.panes.get(&pid).map(|p| p.term_ids.clone()).unwrap_or_default();
            for tid in tids {
                if let Some(tp) = s.term_panes.get_mut(&tid) {
                    terminal::resize_pty(tp, cols, rows);
                }
            }
        }
    }
}

/// Return the SSH host for the active workspace, if any.
/// Used to decide whether new terminals should connect to the remote.
fn active_workspace_ssh_host(s: &State) -> Option<vpath::SshHost> {
    if let Some(host) = s.explorer.as_ref().and_then(|e| e.root.ssh_host().cloned()) {
        return Some(host);
    }

    if let Some(pane) = s.panes.get(&s.active_pane) {
        if pane.kind == PaneKind::Editor {
            if let Some(host) = pane.tabs.get(pane.active)
                .and_then(|tab| tab.path.as_ref())
                .and_then(|path| path.ssh_host().cloned())
            {
                return Some(host);
            }
            if let Some(host) = pane.tabs.iter()
                .filter_map(|tab| tab.path.as_ref())
                .find_map(|path| path.ssh_host().cloned())
            {
                return Some(host);
            }
        }
    }

    s.panes.values()
        .filter(|pane| pane.kind == PaneKind::Editor)
        .flat_map(|pane| pane.tabs.iter())
        .filter_map(|tab| tab.path.as_ref())
        .find_map(|path| path.ssh_host().cloned())
}

/// Returns the directory of the file open in the active editor pane, or the workspace
/// root if no file is open. Returns `None` only if there is no workspace at all.
fn active_editor_dir(s: &State) -> Option<VPath> {
    if let Some(pane) = s.panes.get(&s.active_pane) {
        if pane.kind == PaneKind::Editor {
            if let Some(tab) = pane.tabs.get(pane.active) {
                if let Some(ref vpath) = tab.path {
                    if let Some(parent) = vpath.parent() {
                        return Some(parent);
                    }
                }
            }
        }
    }
    // Fall back to workspace root
    s.explorer.as_ref().map(|ex| ex.root.clone())
}

fn open_terminal_pane(s: &mut State) {
    let cwd = s.explorer.as_ref().map(|ex| ex.root.clone());
    open_terminal_pane_at(s, cwd);
}

fn open_terminal_pane_at(s: &mut State, cwd: Option<VPath>) {
    blur_sidebar(s); // the new terminal gets the keyboard
    let pane_id = s.next_pane_id; s.next_pane_id += 1;
    let term_id = s.next_pane_id; s.next_pane_id += 1;
    let area = s.pane_area();
    let cols = (area.w / s.glyphs.cw).max(1) as usize;
    let rows = ((area.h / 2) / s.glyphs.lh).max(1) as usize;
    let proxy = s.proxy.clone();
    // If we're in a remote workspace, open an SSH session instead of a local shell.
    let tp = if let Some(host) = active_workspace_ssh_host(s) {
        // `ssh … host`, or with a remote cwd `ssh -t … host -- cd <dir> && exec $SHELL -l`
        // (see ssh::terminal_argv). Run directly as an argv, so no local shell
        // re-parses the path or the host.
        let remote_cwd = cwd.as_ref().and_then(|v| match v {
            VPath::Remote { path, .. } => Some(path.as_path()),
            _ => None,
        });
        let ssh_argv = ssh::terminal_argv(&host, remote_cwd);
        terminal::spawn_terminal(term_id, cols, rows, proxy, terminal::TermCommand::Argv(ssh_argv), None)
    } else {
        // Local terminal — pass the local cwd directly to the child.
        let local_cwd = cwd.and_then(|v| v.as_local_path().map(|p| p.to_path_buf()));
        let shell = terminal::TermCommand::shell(&s.settings.terminal_shell);
        terminal::spawn_terminal(term_id, cols, rows, proxy, shell, local_cwd)
    };
    s.term_panes.insert(term_id, tp);
    let pane = Pane { id: pane_id, kind: PaneKind::Terminal, tabs: vec![],
                      term_ids: vec![term_id], active: 0, find: FindBar::new() };
    s.panes.insert(pane_id, pane);
    let active = s.active_pane;
    let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
    s.pane_tree = insert_pane(old_tree, active, pane_id, DropZone::Bottom);
    s.active_pane = pane_id;
}

fn open_markdown_preview(s: &mut State) {
    let Some(pane) = s.panes.get(&s.active_pane) else { return };
    if pane.kind != PaneKind::Editor { return };
    let tab = pane.tab();
    let is_md = tab.path.as_ref()
        .map(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("md" | "markdown")))
        .unwrap_or(false);
    if !is_md { return };
    let source_buf_id = tab.buf_id;
    let title = tab.display_name().to_owned() + " [Preview]";

    if let Some((&pid, _)) = s.md_panes.iter().find(|(_, mp)| mp.source_buf_id == source_buf_id) {
        s.active_pane = pid;
        return;
    }

    let pane_id = s.next_pane_id; s.next_pane_id += 1;
    s.md_panes.insert(pane_id, MarkdownPane { id: pane_id, source_buf_id, scroll: 0, title, lines_cache: Vec::new(), source_edit_gen: u64::MAX });
    s.panes.insert(pane_id, Pane {
        id: pane_id, kind: PaneKind::MarkdownPreview,
        tabs: vec![], term_ids: vec![], active: 0, find: FindBar::new(),
    });
    let active = s.active_pane;
    let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
    s.pane_tree = insert_pane(old_tree, active, pane_id, DropZone::Right);
    s.active_pane = pane_id;
}

fn lsp_binary(lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::TypeScript => Some("typescript-language-server"),
        Lang::Rust       => Some("rust-analyzer"),
        Lang::Python     => Some("pylsp"),
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown | Lang::Css | Lang::Html => None,
    }
}

fn lsp_invocation(lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::TypeScript => Some("typescript-language-server --stdio"),
        Lang::Rust       => Some("rust-analyzer"),
        Lang::Python     => Some("pylsp"),
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown | Lang::Css | Lang::Html => None,
    }
}

fn lsp_required_binaries(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::TypeScript => &["typescript-language-server", "tsserver"],
        Lang::Rust       => &["rust-analyzer"],
        Lang::Python     => &["pylsp"],
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown | Lang::Css | Lang::Html => &[],
    }
}

/// Command typed into a new terminal to install a language server. Ends in `\r`
/// (the Enter key) — PowerShell under ConPTY reads a bare `\n` as Ctrl+J.
fn lsp_install_command(lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::TypeScript => Some("npm install -g typescript-language-server typescript\r"),
        Lang::Rust       => Some("rustup component add rust-analyzer\r"),
        Lang::Python     => Some("pip install python-lsp-server\r"),
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown | Lang::Css | Lang::Html => None,
    }
}

fn remote_lsp_path_dirs(settings: &settings::Settings, host: &vpath::SshHost) -> Vec<String> {
    fn push_unique(dirs: &mut Vec<String>, dir: &str) {
        if !dir.is_empty() && !dirs.iter().any(|existing| existing == dir) {
            dirs.push(dir.to_owned());
        }
    }

    let mut dirs = Vec::new();
    for dir in ["~/.cargo/bin", "~/.local/bin", "~/.npm-global/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        push_unique(&mut dirs, dir);
    }

    for key in [host.display(), host.host_arg(), host.host.clone()] {
        if let Some(extra) = settings.remote_lsp_search_paths.get(&key) {
            for dir in extra {
                push_unique(&mut dirs, dir.as_str());
            }
        }
    }
    dirs
}

fn lsp_scope_host(s: &State) -> Option<vpath::SshHost> {
    active_workspace_ssh_host(s).or_else(|| {
        let pane = s.panes.get(&s.active_pane)?;
        if pane.kind != PaneKind::Editor { return None; }
        pane.tabs.get(pane.active)?.path.as_ref()?.ssh_host().cloned()
    })
}

fn lsp_installed_for(s: &State, lang: Lang, host: Option<&vpath::SshHost>) -> Option<bool> {
    match host {
        Some(h) => s.remote_lsp_installed.get(&(h.clone(), lang)).copied(),
        None    => Some(s.lsp_installed.get(&lang).copied().unwrap_or(false)),
    }
}

fn settings_tab_open(s: &State) -> bool {
    s.panes.values().any(|p| {
        p.kind == PaneKind::Editor && p.tabs.iter().any(|t| t.kind == TabKind::Settings)
    })
}

fn check_lsp_binaries(s: &mut State) {
    let langs = [Lang::TypeScript, Lang::Rust, Lang::Python];
    if let Some(host) = lsp_scope_host(s) {
        if !matches!(s.ssh_connections.get(&host), Some(SshConnectionState::Connected)) {
            s.remote_lsp_check_errors.remove(&host);
            for lang in langs {
                s.remote_lsp_installed.remove(&(host.clone(), lang));
            }
            ssh::ensure_control_master(host, s.proxy.clone());
            return;
        }
        let bins: Vec<(Lang, Vec<String>)> = langs.iter()
            .map(|&lang| (lang, lsp_required_binaries(lang).iter().map(|bin| (*bin).to_owned()).collect()))
            .collect();
        let path_dirs = remote_lsp_path_dirs(&s.settings, &host);
        ssh::ssh_check_lsp_binaries(host, bins, path_dirs, s.proxy.clone());
        return;
    }

    for lang in langs {
        let bins = lsp_required_binaries(lang);
        let installed = !bins.is_empty() && bins.iter().all(|bin| os::find_executable(bin).is_some());
        s.lsp_installed.insert(lang, installed);
    }
}

fn check_lsp_binaries_if_settings_open(s: &mut State) {
    if settings_tab_open(s) {
        check_lsp_binaries(s);
    }
}

fn open_settings_tab(s: &mut State) {
    if s.panes.get(&s.active_pane).map_or(false, |p| p.kind != PaneKind::Editor) { return; }
    check_lsp_binaries(s);
    let pane = s.pane_mut();
    if let Some(i) = pane.tabs.iter().position(|t| t.kind == TabKind::Settings) {
        pane.active = i;
    } else {
        pane.tabs.push(Tab::settings());
        pane.active = pane.tabs.len() - 1;
    }
}

// ── User events (background threads → main loop) ──────────────────────────────
#[derive(Clone, PartialEq)]
pub enum DiagSeverity { Error, Warning, Info, Hint }

#[derive(Clone)]
pub struct Diagnostic {
    pub line:      usize,
    pub col_start: usize,
    pub col_end:   usize,
    pub severity:  DiagSeverity,
    pub message:   String,
}

pub struct MarkdownPane {
    pub id:            usize,
    pub source_buf_id: usize,
    pub scroll:        usize,
    pub title:         String,
    pub lines_cache:   Vec<(String, Vec<u32>)>, // cached rendered lines; rebuilt on source edit
    pub source_edit_gen: u64, // edit_generation of source tab when cache was last built
}

pub enum UserEvent {
    TermOutput     { pane_id: usize, data: Box<[u8]> },
    LspOutput      { pane_id: usize, data: Vec<u8> },
    LspDiagnostics { path: VPath, diagnostics: Vec<Diagnostic> },
    LspResponse    { server_id: usize, id: u64, result: serde_json::Value },
    LspServerStopped { server_id: usize },
    /// Another local-text process asked us (over the control socket) to stop the
    /// server with this `op_id`; reply with the outcome so it can free the slot.
    LspStopRequested { op_id: usize, reply: std::sync::mpsc::Sender<lsp_registry::StopResult> },
    /// A server slot freed up (after we evicted another process's instance) —
    /// start the server we wanted for `root`.
    LspSlotFreed { lang: Lang, ssh_host: Option<vpath::SshHost>, root: Option<VPath> },
    /// Another process asked us to bring our window to the front.
    RaiseWindowRequested,
    LspBinaryCheckResult { host: Option<vpath::SshHost>, statuses: Vec<(Lang, Option<bool>)>, error: Option<String> },
    /// External format command finished; `generation` is the buffer's edit
    /// generation when the save that triggered it happened.
    FormatterDone  { path: VPath, generation: u64 },
    GitStatusResult { staged: Vec<GitEntry>, unstaged: Vec<GitEntry>, is_git_repo: bool, error: Option<String>, repo_root: Option<VPath> },
    GitOpFailed     { msg: String },
    GitDiffResult   { buf_id: usize, path: String, lines: Vec<DiffLine> },
    GitOpDone,
    SearchDone        { token: u64, results: Vec<GlobalFindResult> },
    QuickFinderFiles  { token: u64, entries: Vec<VPath> },
    TreeSearchFiles   { root: PathBuf, entries: Vec<PathBuf> },
    Redraw,
    // SSH / remote events
    SshConnecting       { host: vpath::SshHost },
    SshConnected        { host: vpath::SshHost },
    SshError            { host: vpath::SshHost, msg: String },
    RemoteFileContent   { token: u64, path: VPath, content: String },
    RemoteFileError     { token: u64, path: VPath, msg: String },
    RemoteWriteDone     { path: VPath, generation: u64 },
    RemoteWriteError    { path: VPath, msg: String },
    RemoteDirListing    { path: VPath, entries: Vec<(String, bool)> },
    RemoteDirListingError { path: VPath, msg: String },
    RemoteFsOpDone {
        refresh_dir: VPath,
        renamed: Option<(VPath, VPath)>,
        open_path: Option<VPath>,
    },
    RemoteFsOpError { path: VPath, msg: String },
    // Collab events
    CollabMessage     { from_site_id: u64, msg: collab::CollabMsg },
    CollabConnected   { session: Box<collab::CollabSession>, doc: Option<collab::DocSnapshot> },
    /// Host: the file list for a guest finished building (background walk).
    CollabFileListReady { site_id: u64, paths: Vec<String> },
    CollabDisconnected,
    CollabError       { msg: String },
    CollabGuestJoined { site_id: u64, peer: collab::PeerInfo },
    CollabGuestLeft   { site_id: u64 },
}

// ── Application state ─────────────────────────────────────────────────────────
struct State {
    win:      Arc<Window>,
    renderer: platform::Renderer,
    w: u32, h: u32,

    pane_tree:    PaneTree,
    panes:        HashMap<usize, Pane>,
    active_pane:  usize,
    next_pane_id: usize,
    next_buf_id:  usize,
    next_remote_read_token: u64,
    drag:         Option<DragState>,
    drag_pending: Option<(usize, usize, f32, f32)>,
    resize_drag:  Option<ResizeDrag>,

    term_panes:     HashMap<usize, terminal::TermPane>,
    md_panes:       HashMap<usize, MarkdownPane>,
    lsp:            lsp::LspManager,
    diagnostics:    HashMap<VPath, Vec<Diagnostic>>,
    lsp_installed:  HashMap<Lang, bool>,
    remote_lsp_installed: HashMap<(vpath::SshHost, Lang), bool>,
    remote_lsp_check_errors: HashMap<vpath::SshHost, String>,
    lsp_start_errors: HashMap<(Option<vpath::SshHost>, Lang), String>,
    proxy:          EventLoopProxy<UserEvent>,

    cursor_visible: bool,
    cursor_blink:   Instant,
    mods:   ModifiersState,

    font_size: f32,
    glyphs:     Glyphs,

    explorer:      Option<FileExplorer>,
    explorer_w:    i32,   // sidebar pixel width (default 200)
    explorer_drag: bool,  // true while dragging the explorer right border
    mouse_x:    f32,
    mouse_y:    f32,
    mouse_down:       bool,
    term_buttons_held: u8,  // bitmask: bit0=left, bit1=mid, bit2=right
    term_sel:    Option<TermSel>,  // text selection in the active terminal
    term_selecting: bool,          // true while left-drag selecting in terminal
    term_click_count:     u32,
    term_last_click_time: Instant,
    term_last_click_row:  usize,
    term_last_click_col:  usize,
    last_click_time: Instant,
    last_click_char: usize,
    click_count:     u32,

    settings:     settings::Settings,
    needs_redraw: bool,

    settings_edit_field:  Option<SettingsFieldId>,
    settings_edit_text:   String,
    settings_edit_cursor: usize,

    cursor_back: Vec<(VPath, usize)>,
    cursor_fwd:  Vec<(VPath, usize)>,

    left_view:          LeftView,
    left_panel_visible: bool,
    /// The visible sidebar list (Diagnostics, or Search results while no search
    /// field is focused) owns the navigation keys (Up/Down/Enter/Escape).
    sidebar_list_focused: bool,
    diag_panel_sel: usize,
    context_menu: Option<ContextMenu>,
    explorer_clipboard: Option<VPath>,
    lsp_panel:    Option<LspManagerPanel>,
    quick_finder: QuickFinder,
    global_find:  GlobalFind,
    git_panel:    GitPanel,
    git_diff_tabs: HashMap<usize, GitDiffTabData>,
    scroll_frac_y: f64,  // fractional pixel accumulator for PixelDelta scroll events

    status_msg: Option<String>,

    last_sync_pane: usize,  // active pane at last sibling-tab sync
    last_sync_tab:  usize,  // active tab index at last sibling-tab sync
    last_sync_gen:  u64,    // edit_generation of active tab at last sync

    /// Connection state for each SSH host (Connecting | Connected | Failed).
    ssh_connections: HashMap<vpath::SshHost, SshConnectionState>,

    /// Active collab session (None = not in a session, zero RAM overhead).
    collab: Option<collab::CollabSession>,
    /// Before-snapshot captured in push_undo when collab is active, consumed by notify_collab_change.
    collab_before: Option<(usize, ropey::Rope)>, // (buf_id, text before the local edit)
    /// Linux without zenity/kdialog: "close again to discard" confirmation —
    /// what was armed and when.
    #[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
    discard_armed: Option<(String, Instant)>,
    /// File list sent by the host (guest-side only); empty when hosting or not in a session.
    collab_file_list: Vec<String>,
}

#[derive(Clone, PartialEq)]
enum SshConnectionState {
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Clone)]
struct TermSel {
    start_vi:  usize,  // visual row in visible_rows()
    start_col: usize,
    end_vi:    usize,
    end_col:   usize,
}

impl TermSel {
    /// Normalize so start <= end in reading order.
    fn normalized(&self) -> (usize, usize, usize, usize) {
        let (r0, c0, r1, c1) = (self.start_vi, self.start_col, self.end_vi, self.end_col);
        if r0 < r1 || (r0 == r1 && c0 <= c1) { (r0, c0, r1, c1) } else { (r1, c1, r0, c0) }
    }
    fn contains(&self, vi: usize, col: usize) -> bool {
        let (r0, c0, r1, c1) = self.normalized();
        if vi < r0 || vi > r1 { return false; }
        if vi == r0 && col < c0 { return false; }
        if vi == r1 && col > c1 { return false; }
        true
    }
    fn is_empty(&self) -> bool {
        self.start_vi == self.end_vi && self.start_col == self.end_col
    }
}

fn clipboard_write(text: &str) { clipboard_set(text); }
fn clipboard_read() -> String { clipboard_get().unwrap_or_default() }

impl State {
    fn pane(&self)         -> &Pane     { &self.panes[&self.active_pane] }
    fn pane_mut(&mut self) -> &mut Pane { let id = self.active_pane; self.panes.get_mut(&id).unwrap() }
    fn tab(&self)          -> &Tab      { self.pane().tab() }
    fn tab_mut(&mut self)  -> &mut Tab  { let id = self.active_pane; self.panes.get_mut(&id).unwrap().tab_mut() }
    fn find(&self)         -> &FindBar  { &self.pane().find }
    fn find_mut(&mut self) -> &mut FindBar { let id = self.active_pane; &mut self.panes.get_mut(&id).unwrap().find }

    /// Ensure `active_pane` refers to a live pane. Pane-removal paths keep this in sync, but
    /// if one ever leaves it dangling, re-point to an existing pane so the many
    /// `panes[&active_pane]` accessors above can't panic. `panes` always has ≥1 entry.
    fn ensure_active_pane(&mut self) {
        if !self.panes.contains_key(&self.active_pane) {
            if let Some(&id) = self.panes.keys().next() {
                self.active_pane = id;
            }
        }
    }

    fn activity_bar_w(&self) -> i32 {
        if self.explorer.is_some() { self.glyphs.cw * 4 + 8 } else { 0 }
    }

    fn explorer_w(&self) -> i32 {
        if self.explorer.is_some() && self.left_panel_visible { self.explorer_w } else { 0 }
    }

    fn tab_h(&self)    -> i32 { self.glyphs.lh + 4 }
    fn status_h(&self) -> i32 { self.glyphs.lh + 4 }

    fn line_num_digits(total: usize) -> usize {
        let mut d = 1;
        let mut x = total.max(1);
        while x >= 10 { d += 1; x /= 10; }
        d.max(2)
    }
    // Width of the line-number gutter: digits × cw + ED_LPAD gap before text.
    fn gutter_w(total: usize, cw: i32) -> i32 {
        Self::line_num_digits(total) as i32 * cw + ED_LPAD
    }
    fn active_gutter_w(&self) -> i32 {
        Self::gutter_w(self.tab().text.len_lines(), self.glyphs.cw)
    }

    fn pane_find_h(pane: &Pane, lh: i32) -> i32 {
        if !pane.find.open { return 0; }
        let row_h = lh + 4;
        if pane.find.replace_open { row_h * 2 } else { row_h }
    }
    fn find_h(&self) -> i32 {
        if !self.panes.contains_key(&self.active_pane) { return 0; }
        Self::pane_find_h(self.pane(), self.glyphs.lh)
    }

    fn pane_area(&self) -> Rect {
        let act_w = self.activity_bar_w();
        let ew    = self.explorer_w();
        Rect { x: act_w + ew, y: 0, w: self.w as i32 - act_w - ew, h: self.h as i32 - self.status_h() }
    }

    fn active_pane_rect(&self) -> Rect {
        let area = self.pane_area();
        layout_tree(&self.pane_tree, area)
            .into_iter()
            .find(|(id, _)| *id == self.active_pane)
            .map(|(_, r)| r)
            .unwrap_or(area)
    }

    fn editor_h(&self) -> i32 {
        let r  = self.active_pane_rect();
        let fh = self.find_h();
        r.h - self.tab_h() - fh
    }

    fn cursor_lc(&self) -> (usize, usize) {
        let t    = self.tab();
        let c    = t.primary().head.min(t.text.len_chars());
        let line = t.text.char_to_line(c);
        let col  = c - t.text.line_to_char(line);
        (line, col)
    }

    fn line_len(rope: &Rope, line: usize) -> usize {
        let s = rope.line(line);
        let n = s.len_chars();
        if n > 0 && s.char(n - 1) == '\n' { n - 1 } else { n }
    }

    fn last_line(rope: &Rope) -> usize {
        let n = rope.len_lines();
        if n == 0 { return 0; }
        if rope.len_chars() > 0 && rope.char(rope.len_chars() - 1) == '\n' {
            n.saturating_sub(2)
        } else {
            n - 1
        }
    }

    fn ensure_visible(&mut self) {
        let r   = self.active_pane_rect();
        let fh  = { let ap = self.active_pane; Self::pane_find_h(&self.panes[&ap], self.glyphs.lh) };
        let eh  = r.h - self.tab_h() - fh;
        let vis_v = (eh / self.glyphs.lh).max(1) as usize;
        let gw    = { let ap = self.active_pane; Self::gutter_w(self.panes[&ap].tab().text.len_lines(), self.glyphs.cw) };
        let vis_h = ((r.w - gw) / self.glyphs.cw).max(1) as usize;
        let (line, col) = self.cursor_lc();
        let id = self.active_pane;
        let t = self.panes.get_mut(&id).unwrap().tab_mut();
        if line < t.scroll              { t.scroll  = line; }
        if line >= t.scroll  + vis_v   { t.scroll  = (line + 1).saturating_sub(vis_v); }
        if col  < t.hscroll             { t.hscroll = col; }
        if col  >= t.hscroll + vis_h   { t.hscroll = (col  + 1).saturating_sub(vis_h); }
    }

    // ── Multi-cursor helpers ──────────────────────────────────────────────────

    // Returns cursor indices sorted by lo() ascending (left-to-right order).
    fn cursor_order_ltr(&self) -> Vec<usize> {
        let n = self.tab().cursors.len();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| self.tab().cursors[i].lo());
        order
    }

    // Merge/deduplicate cursors after edits or additions.
    fn dedup_cursors(&mut self) {
        let t = self.tab_mut();
        t.cursors.sort_by_key(|c| c.lo());
        let mut merged: Vec<Cursor> = Vec::new();
        for c in t.cursors.drain(..) {
            if let Some(last) = merged.last_mut() {
                if c.lo() <= last.hi() {
                    if c.hi() > last.hi() { last.head = c.head; }
                    continue;
                }
            }
            merged.push(c);
        }
        if merged.is_empty() { merged.push(Cursor::new(0)); }
        t.cursors = merged;
    }

    // ── Undo / Redo ───────────────────────────────────────────────────────────

    // Call before every edit. coalesce=true merges with the previous entry when
    // that entry was also coalesced (used for consecutive single-char typing).
    fn push_undo(&mut self, coalesce: bool) {
        // In collab mode: capture the before-snapshot (once per event batch) for op extraction,
        // then do hl/gen invalidation but skip the undo stack — full-snapshot undo is unsafe
        // in a shared document (it would revert peer edits).
        if self.collab.is_some() {
            if self.collab_before.is_none() {
                let tab = self.tab();
                self.collab_before = Some((tab.buf_id, tab.text.clone()));
            }
            let tab = self.tab_mut();
            tab.note_edit();
            tab.last_typing = coalesce;
            return;
        }
        let limit = self.settings.undo_limit;
        let tab = self.tab_mut();
        tab.note_edit();
        if !coalesce || !tab.last_typing {
            if let Some(lim) = limit {
                if tab.undo_stack.len() >= lim && !tab.undo_stack.is_empty() { tab.undo_stack.remove(0); }
            }
            tab.undo_stack.push(UndoEntry { text: tab.text.clone(), cursors: tab.cursors.clone() });
            tab.redo_stack.clear();
        }
        tab.last_typing = coalesce;
    }

    fn undo(&mut self) {
        if self.collab.is_some() {
            self.status_msg = Some("Undo is disabled during a collab session".to_owned());
            return;
        }
        let limit = self.settings.undo_limit;
        let Some(entry) = self.tab_mut().undo_stack.pop() else { return };
        let cur = UndoEntry { text: self.tab().text.clone(), cursors: self.tab().cursors.clone() };
        let tab = self.tab_mut();
        if let Some(lim) = limit { if tab.redo_stack.len() >= lim && !tab.redo_stack.is_empty() { tab.redo_stack.remove(0); } }
        tab.redo_stack.push(cur);
        tab.note_edit();
        tab.text    = entry.text;
        tab.cursors = entry.cursors;
        tab.clamp_cursors();
        tab.dirty   = true;
        tab.last_typing = false;
        self.ensure_visible();
    }

    fn redo(&mut self) {
        if self.collab.is_some() { return; }
        let limit = self.settings.undo_limit;
        let Some(entry) = self.tab_mut().redo_stack.pop() else { return };
        let cur = UndoEntry { text: self.tab().text.clone(), cursors: self.tab().cursors.clone() };
        let tab = self.tab_mut();
        if let Some(lim) = limit { if tab.undo_stack.len() >= lim && !tab.undo_stack.is_empty() { tab.undo_stack.remove(0); } }
        tab.undo_stack.push(cur);
        tab.note_edit();
        tab.text    = entry.text;
        tab.cursors = entry.cursors;
        tab.clamp_cursors();
        tab.dirty   = true;
        tab.last_typing = false;
        self.ensure_visible();
    }

    // ── Editing ───────────────────────────────────────────────────────────────

    fn insert_str(&mut self, text: &str) {
        let n_chars = text.chars().count();
        // Coalesce consecutive single printable char inserts with no selection.
        let is_single = n_chars == 1 && !matches!(text, "\n" | "\t");
        let no_sel = !self.tab().cursors.iter().any(|c| c.has_sel());
        self.push_undo(is_single && no_sel);
        let order = self.cursor_order_ltr();
        let mut delta: isize = 0;
        for &i in &order {
            let (orig_lo, orig_hi) = {
                let c = &self.tab().cursors[i];
                (c.lo(), c.hi())
            };
            let lo = apply_delta(orig_lo, delta, self.tab().text.len_chars());
            let hi = apply_delta(orig_hi, delta, self.tab().text.len_chars());
            if lo < hi {
                self.tab_mut().text.remove(lo..hi);
                delta -= (hi - lo) as isize;
            }
            let pos = lo.min(self.tab().text.len_chars());
            self.tab_mut().text.insert(pos, text);
            self.tab_mut().cursors[i] = Cursor::new(pos + n_chars);
            delta += n_chars as isize;
            self.tab_mut().dirty = true;
        }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn insert_str_per_cursor(&mut self, texts: &[String]) {
        self.push_undo(false);
        let order = self.cursor_order_ltr();
        let mut delta: isize = 0;
        for (k, &i) in order.iter().enumerate() {
            let text = texts.get(k).map(|s| s.as_str()).unwrap_or("");
            let n_chars = text.chars().count();
            let (orig_lo, orig_hi) = { let c = &self.tab().cursors[i]; (c.lo(), c.hi()) };
            let lo = apply_delta(orig_lo, delta, self.tab().text.len_chars());
            let hi = apply_delta(orig_hi, delta, self.tab().text.len_chars());
            if lo < hi {
                self.tab_mut().text.remove(lo..hi);
                delta -= (hi - lo) as isize;
            }
            let pos = lo.min(self.tab().text.len_chars());
            if !text.is_empty() { self.tab_mut().text.insert(pos, text); }
            self.tab_mut().cursors[i] = Cursor::new(pos + n_chars);
            delta += n_chars as isize;
            self.tab_mut().dirty = true;
        }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn insert_newline(&mut self) {
        if self.tab().lang() != Lang::Python {
            self.insert_str("\n");
            return;
        }
        let insertions: Vec<String> = self.cursor_order_ltr().into_iter().map(|i| {
            let cursor = &self.tab().cursors[i];
            let pos = cursor.lo().min(self.tab().text.len_chars());
            let line_idx = self.tab().text.char_to_line(pos);
            let line_start = self.tab().text.line_to_char(line_idx);
            let before: String = self.tab().text.slice(line_start..pos).chars().collect();
            let indent: String = before.chars()
                .take_while(|c| matches!(c, ' ' | '\t'))
                .collect();
            let extra = if python_line_opens_block(&before) { "    " } else { "" };
            format!("\n{indent}{extra}")
        }).collect();
        self.insert_str_per_cursor(&insertions);
    }

    fn backspace(&mut self) {
        self.push_undo(false);
        let order = self.cursor_order_ltr();
        let mut delta: isize = 0;
        for &i in &order {
            let (has_sel, orig_lo, orig_hi, orig_head) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi(), c.head)
            };
            let lo   = apply_delta(orig_lo, delta, self.tab().text.len_chars());
            let hi   = apply_delta(orig_hi, delta, self.tab().text.len_chars());
            let head = apply_delta(orig_head, delta, self.tab().text.len_chars());
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                delta -= (hi - lo) as isize;
                self.tab_mut().dirty = true;
            } else if head > 0 {
                let c = head.min(self.tab().text.len_chars());
                self.tab_mut().text.remove(c - 1..c);
                self.tab_mut().cursors[i] = Cursor::new(c - 1);
                delta -= 1;
                self.tab_mut().dirty = true;
            }
        }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn delete_fwd(&mut self) {
        self.push_undo(false);
        let order = self.cursor_order_ltr();
        let mut delta: isize = 0;
        for &i in &order {
            let (has_sel, orig_lo, orig_hi, orig_head) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi(), c.head)
            };
            let lo   = apply_delta(orig_lo, delta, self.tab().text.len_chars());
            let hi   = apply_delta(orig_hi, delta, self.tab().text.len_chars());
            let head = apply_delta(orig_head, delta, self.tab().text.len_chars());
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                delta -= (hi - lo) as isize;
                self.tab_mut().dirty = true;
            } else {
                let c = head.min(self.tab().text.len_chars());
                if c < self.tab().text.len_chars() {
                    self.tab_mut().text.remove(c..c + 1);
                    self.tab_mut().cursors[i] = Cursor::new(c);
                    delta -= 1;
                    self.tab_mut().dirty = true;
                }
            }
        }
        self.dedup_cursors();
    }

    fn delete_word_back(&mut self) {
        self.push_undo(false);
        let order = self.cursor_order_ltr();
        let mut delta: isize = 0;
        for &i in &order {
            let (has_sel, orig_lo, orig_hi, orig_head) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi(), c.head)
            };
            let lo   = apply_delta(orig_lo, delta, self.tab().text.len_chars());
            let hi   = apply_delta(orig_hi, delta, self.tab().text.len_chars());
            let head = apply_delta(orig_head, delta, self.tab().text.len_chars());
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                delta -= (hi - lo) as isize;
                self.tab_mut().dirty = true;
            } else {
                let end = head.min(self.tab().text.len_chars());
                let mut start = end;
                while start > 0 && !Self::is_word_char(self.tab().text.char(start - 1)) { start -= 1; }
                while start > 0 &&  Self::is_word_char(self.tab().text.char(start - 1)) { start -= 1; }
                if start < end {
                    self.tab_mut().text.remove(start..end);
                    self.tab_mut().cursors[i] = Cursor::new(start);
                    delta -= (end - start) as isize;
                    self.tab_mut().dirty = true;
                    // A backwards word delete can reach past cursors already processed
                    // (they sit to the left). Pull them back to the deletion start so
                    // they don't end up past the end of the shortened text.
                    for &j in &order {
                        if j == i { break; }
                        let c = &mut self.tab_mut().cursors[j];
                        if c.head > start { c.head = start; }
                        if c.tail > start { c.tail = start; }
                    }
                }
            }
        }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn delete_to_line_start(&mut self) {
        self.push_undo(false);
        let order = self.cursor_order_ltr();
        let mut delta: isize = 0;
        for &i in &order {
            let (has_sel, orig_lo, orig_hi, orig_head) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi(), c.head)
            };
            let lo   = apply_delta(orig_lo, delta, self.tab().text.len_chars());
            let hi   = apply_delta(orig_hi, delta, self.tab().text.len_chars());
            let head = apply_delta(orig_head, delta, self.tab().text.len_chars());
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                delta -= (hi - lo) as isize;
                self.tab_mut().dirty = true;
            } else {
                let cursor_pos = head.min(self.tab().text.len_chars());
                let line = self.tab().text.char_to_line(cursor_pos);
                let start = self.tab().text.line_to_char(line);
                if start < cursor_pos {
                    self.tab_mut().text.remove(start..cursor_pos);
                    self.tab_mut().cursors[i] = Cursor::new(start);
                    delta -= (cursor_pos - start) as isize;
                    self.tab_mut().dirty = true;
                } else if start > 0 {
                    self.tab_mut().text.remove(start - 1..start);
                    self.tab_mut().cursors[i] = Cursor::new(start - 1);
                    delta -= 1;
                    self.tab_mut().dirty = true;
                }
            }
        }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn delete_word_fwd(&mut self) {
        self.push_undo(false);
        let order = self.cursor_order_ltr();
        let mut delta: isize = 0;
        for &i in &order {
            let (has_sel, orig_lo, orig_hi, orig_head) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi(), c.head)
            };
            let lo   = apply_delta(orig_lo, delta, self.tab().text.len_chars());
            let hi   = apply_delta(orig_hi, delta, self.tab().text.len_chars());
            let head = apply_delta(orig_head, delta, self.tab().text.len_chars());
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                delta -= (hi - lo) as isize;
                self.tab_mut().dirty = true;
            } else {
                let len = self.tab().text.len_chars();
                let start = head.min(len);
                let mut end = start;
                while end < len && !Self::is_word_char(self.tab().text.char(end)) { end += 1; }
                while end < len &&  Self::is_word_char(self.tab().text.char(end)) { end += 1; }
                if end > start {
                    self.tab_mut().text.remove(start..end);
                    self.tab_mut().cursors[i] = Cursor::new(start);
                    delta -= (end - start) as isize;
                    self.tab_mut().dirty = true;
                }
            }
        }
        self.dedup_cursors();
    }

    fn delete_to_line_end(&mut self) {
        self.push_undo(false);
        let order = self.cursor_order_ltr();
        let mut delta: isize = 0;
        for &i in &order {
            let (has_sel, orig_lo, orig_hi, orig_head) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi(), c.head)
            };
            let lo   = apply_delta(orig_lo, delta, self.tab().text.len_chars());
            let hi   = apply_delta(orig_hi, delta, self.tab().text.len_chars());
            let head = apply_delta(orig_head, delta, self.tab().text.len_chars());
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                delta -= (hi - lo) as isize;
                self.tab_mut().dirty = true;
            } else {
                let len = self.tab().text.len_chars();
                let c = head.min(len);
                let l = self.tab().text.char_to_line(c);
                let line_end = self.tab().text.line_to_char(l) + Self::line_len(&self.tab().text, l);
                if line_end > c {
                    self.tab_mut().text.remove(c..line_end);
                    self.tab_mut().cursors[i] = Cursor::new(c);
                    delta -= (line_end - c) as isize;
                    self.tab_mut().dirty = true;
                } else if c < len {
                    self.tab_mut().text.remove(c..c + 1);
                    self.tab_mut().cursors[i] = Cursor::new(c);
                    delta -= 1;
                    self.tab_mut().dirty = true;
                }
            }
        }
        self.dedup_cursors();
    }

    /// Line range (inclusive) covered by each cursor, merged and sorted. A selection
    /// ending at column 0 of a line doesn't include that line.
    fn cursor_line_ranges(&self) -> Vec<(usize, usize)> {
        let t = self.tab();
        let len = t.text.len_chars();
        let mut ranges: Vec<(usize, usize)> = t.cursors.iter().map(|c| {
            let lo = c.lo().min(len);
            let hi = c.hi().min(len);
            let first = t.text.char_to_line(lo);
            let mut last = t.text.char_to_line(hi);
            if hi > lo && last > first && hi == t.text.line_to_char(last) { last -= 1; }
            (first, last)
        }).collect();
        ranges.sort();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (a, b) in ranges {
            match merged.last_mut() {
                Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        merged
    }

    /// Tab: with a selection spanning lines, indent those lines; otherwise insert
    /// spaces at each cursor (replacing a selection within a single line).
    fn indent(&mut self) {
        let multi_line = self.tab().cursors.iter().any(|c| {
            c.has_sel() && {
                let t = self.tab();
                let len = t.text.len_chars();
                t.text.char_to_line(c.lo().min(len)) != t.text.char_to_line(c.hi().min(len))
            }
        });
        if !multi_line {
            self.insert_str("    ");
            return;
        }
        let ranges = self.cursor_line_ranges();
        self.push_undo(false);
        // Bottom-up so earlier line starts stay valid.
        let mut inserted_at: Vec<usize> = Vec::new();
        for &(first, last) in ranges.iter().rev() {
            for line in (first..=last).rev() {
                if State::line_len(&self.tab().text, line) == 0 { continue; } // leave blank lines alone
                let at = self.tab().text.line_to_char(line);
                self.tab_mut().text.insert(at, "    ");
                inserted_at.push(at);
            }
        }
        // Shift cursors: every insertion at or before a position moves it right by 4.
        inserted_at.sort_unstable();
        let shift = |pos: usize| pos + 4 * inserted_at.partition_point(|&a| a <= pos);
        for c in &mut self.tab_mut().cursors {
            c.head = shift(c.head);
            c.tail = shift(c.tail);
        }
        self.tab_mut().dirty = true;
        self.dedup_cursors();
        self.ensure_visible();
    }

    /// Shift+Tab: remove up to 4 leading spaces (or one tab) from each line the
    /// cursors touch.
    fn dedent(&mut self) {
        let ranges = self.cursor_line_ranges();
        let mut removals: Vec<(usize, usize)> = Vec::new(); // (start char, count)
        {
            let t = &self.tab().text;
            for &(first, last) in &ranges {
                for line in first..=last {
                    let start = t.line_to_char(line);
                    let mut n = 0;
                    for ch in t.line(line).chars().take(4) {
                        if ch == ' ' { n += 1; } else { if ch == '\t' && n == 0 { n = 1; } break; }
                    }
                    if n > 0 { removals.push((start, n)); }
                }
            }
        }
        if removals.is_empty() { return; }
        self.push_undo(false);
        for &(start, n) in removals.iter().rev() {
            self.tab_mut().text.remove(start..start + n);
        }
        // Map cursor positions through the removals (all ascending by start).
        let map = |pos: usize| {
            let mut out = pos;
            for &(start, n) in &removals {
                if start >= pos { break; }
                out -= n.min(pos - start);
            }
            out
        };
        for c in &mut self.tab_mut().cursors {
            c.head = map(c.head);
            c.tail = map(c.tail);
        }
        self.tab_mut().dirty = true;
        self.dedup_cursors();
        self.ensure_visible();
    }

    // ── Cursor movement ───────────────────────────────────────────────────────

    // When !selecting, tail is synced to head so no selection remains.
    // When selecting, tail (the anchor) stays fixed; only head moves.
    fn move_left(&mut self, selecting: bool) {
        for c in &mut self.tab_mut().cursors { if c.head > 0 { c.head -= 1; } }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn move_right(&mut self, selecting: bool) {
        let n = self.tab().text.len_chars();
        for c in &mut self.tab_mut().cursors { if c.head < n { c.head += 1; } }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn move_up(&mut self, selecting: bool) {
        let n = self.tab().cursors.len();
        let mut new_heads = Vec::with_capacity(n);
        for i in 0..n {
            let pos  = self.tab().cursors[i].head.min(self.tab().text.len_chars());
            let line = self.tab().text.char_to_line(pos);
            let col  = pos - self.tab().text.line_to_char(line);
            let h = if line == 0 { 0 } else {
                let prev = line - 1;
                self.tab().text.line_to_char(prev) + col.min(Self::line_len(&self.tab().text, prev))
            };
            new_heads.push(h);
        }
        for (i, h) in new_heads.into_iter().enumerate() { self.tab_mut().cursors[i].head = h; }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn move_down(&mut self, selecting: bool) {
        let n = self.tab().cursors.len();
        let mut new_heads = Vec::with_capacity(n);
        for i in 0..n {
            let pos  = self.tab().cursors[i].head.min(self.tab().text.len_chars());
            let line = self.tab().text.char_to_line(pos);
            let col  = pos - self.tab().text.line_to_char(line);
            let last = Self::last_line(&self.tab().text);
            let h = if line >= last { self.tab().text.len_chars() } else {
                let next = line + 1;
                self.tab().text.line_to_char(next) + col.min(Self::line_len(&self.tab().text, next))
            };
            new_heads.push(h);
        }
        for (i, h) in new_heads.into_iter().enumerate() { self.tab_mut().cursors[i].head = h; }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn add_cursor_above(&mut self) {
        let pos  = self.tab().primary().head.min(self.tab().text.len_chars());
        let line = self.tab().text.char_to_line(pos);
        if line == 0 { return; }
        let col  = pos - self.tab().text.line_to_char(line);
        let prev = line - 1;
        let new_pos = self.tab().text.line_to_char(prev)
            + col.min(Self::line_len(&self.tab().text, prev));
        self.tab_mut().cursors.push(Cursor::new(new_pos));
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn add_cursor_below(&mut self) {
        let pos  = self.tab().primary().head.min(self.tab().text.len_chars());
        let line = self.tab().text.char_to_line(pos);
        let last = Self::last_line(&self.tab().text);
        if line >= last { return; }
        let col  = pos - self.tab().text.line_to_char(line);
        let next = line + 1;
        let new_pos = self.tab().text.line_to_char(next)
            + col.min(Self::line_len(&self.tab().text, next));
        self.tab_mut().cursors.push(Cursor::new(new_pos));
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn move_home(&mut self, selecting: bool) {
        let n = self.tab().cursors.len();
        let mut new_heads = Vec::with_capacity(n);
        for i in 0..n {
            let pos  = self.tab().cursors[i].head.min(self.tab().text.len_chars());
            let line = self.tab().text.char_to_line(pos);
            new_heads.push(self.tab().text.line_to_char(line));
        }
        for (i, h) in new_heads.into_iter().enumerate() { self.tab_mut().cursors[i].head = h; }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn move_end(&mut self, selecting: bool) {
        let n = self.tab().cursors.len();
        let mut new_heads = Vec::with_capacity(n);
        for i in 0..n {
            let pos  = self.tab().cursors[i].head.min(self.tab().text.len_chars());
            let line = self.tab().text.char_to_line(pos);
            new_heads.push(self.tab().text.line_to_char(line) + Self::line_len(&self.tab().text, line));
        }
        for (i, h) in new_heads.into_iter().enumerate() { self.tab_mut().cursors[i].head = h; }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn move_word_left(&mut self, selecting: bool) {
        let n = self.tab().cursors.len();
        let mut new_heads = Vec::with_capacity(n);
        for i in 0..n {
            let mut pos = self.tab().cursors[i].head.min(self.tab().text.len_chars());
            while pos > 0 && !Self::is_word_char(self.tab().text.char(pos - 1)) { pos -= 1; }
            while pos > 0 &&  Self::is_word_char(self.tab().text.char(pos - 1)) { pos -= 1; }
            new_heads.push(pos);
        }
        for (i, h) in new_heads.into_iter().enumerate() { self.tab_mut().cursors[i].head = h; }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn move_word_right(&mut self, selecting: bool) {
        let n = self.tab().cursors.len();
        let mut new_heads = Vec::with_capacity(n);
        for i in 0..n {
            let len = self.tab().text.len_chars();
            let mut pos = self.tab().cursors[i].head.min(len);
            while pos < len && !Self::is_word_char(self.tab().text.char(pos)) { pos += 1; }
            while pos < len &&  Self::is_word_char(self.tab().text.char(pos)) { pos += 1; }
            new_heads.push(pos);
        }
        for (i, h) in new_heads.into_iter().enumerate() { self.tab_mut().cursors[i].head = h; }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn move_doc_start(&mut self, selecting: bool) {
        for c in &mut self.tab_mut().cursors { c.head = 0; }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn move_doc_end(&mut self, selecting: bool) {
        let n = self.tab().text.len_chars();
        for c in &mut self.tab_mut().cursors { c.head = n; }
        if !selecting { for c in &mut self.tab_mut().cursors { c.tail = c.head; } }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn reset_blink(&mut self) {
        self.cursor_visible = true;
        self.cursor_blink = Instant::now() + Duration::from_millis(500);
    }

    fn is_word_char(c: char) -> bool { c.is_alphanumeric() || c == '_' }

    fn scroll_by(&mut self, delta: i32) {
        let (kind, buf_id) = { let t = self.tab(); (t.kind.clone(), t.buf_id) };
        let last = if kind == TabKind::GitDiff {
            self.git_diff_tabs.get(&buf_id).map_or(0, |d| d.lines.len().saturating_sub(1))
        } else {
            let t = self.tab();
            Self::last_line(&t.text)
        };
        let t = self.tab_mut();
        if delta < 0 {
            t.scroll = t.scroll.saturating_sub((-delta) as usize);
        } else {
            t.scroll = (t.scroll + delta as usize).min(last);
        }
    }

    fn hscroll_by(&mut self, delta: i32) {
        let r  = self.active_pane_rect();
        let gw = self.active_gutter_w();
        let vis_h = ((r.w - gw) / self.glyphs.cw).max(1) as usize;
        let max_len = (0..self.tab().text.len_lines())
            .map(|li| Self::line_len(&self.tab().text, li))
            .max()
            .unwrap_or(0);
        let max_hscroll = max_len.saturating_sub(vis_h);
        let t = self.tab_mut();
        if delta < 0 {
            t.hscroll = t.hscroll.saturating_sub((-delta) as usize);
        } else {
            t.hscroll = (t.hscroll + delta as usize).min(max_hscroll);
        }
    }

    fn rebuild_glyphs(&mut self) {
        self.glyphs.resize(self.font_size);
    }

    fn xy_to_char(&self, mx: i32, my: i32) -> usize {
        let r     = self.active_pane_rect();
        let tab_h = self.tab_h();
        let lh    = self.glyphs.lh;
        let cw    = self.glyphs.cw;
        let ed_x  = r.x + Self::gutter_w(self.tab().text.len_lines(), cw);
        let vi    = ((my - r.y - tab_h).max(0) / lh) as usize;
        let t     = self.tab();
        let li    = (t.scroll + vi).min(t.text.len_lines().saturating_sub(1));
        let col   = ((mx - ed_x).max(0) / cw) as usize + t.hscroll;
        t.text.line_to_char(li) + col.min(Self::line_len(&t.text, li))
    }
}

// ── Helper: open file in a tab ────────────────────────────────────────────────
fn begin_remote_tab_read(s: &mut State, pane_id: usize, tab_idx: usize, path: VPath) {
    let VPath::Remote { host, path: remote_path } = path.clone() else { return; };

    if !matches!(s.ssh_connections.get(&host), Some(SshConnectionState::Connected)) {
        s.status_msg = Some(format!("Connecting to {}…", host.display()));
        ssh::ensure_control_master(host, s.proxy.clone());
        return;
    }

    let token = s.next_remote_read_token;
    s.next_remote_read_token += 1;
    let Some(tab) = s.panes.get_mut(&pane_id).and_then(|p| p.tabs.get_mut(tab_idx)) else {
        return;
    };
    if tab.path.as_ref() != Some(&path) {
        return;
    }
    tab.remote_load_token = Some(token);
    ssh::ssh_read_file(host, remote_path, token, s.proxy.clone());
}

fn open_or_reuse_tab(s: &mut State, path: VPath) {
    // Find the nearest editor pane to open the file in (active if it's an editor,
    // otherwise the first editor pane in the layout), creating one if none exist.
    let ap = ensure_editor_pane(s);
    {
        let pane = s.panes.get_mut(&ap).unwrap();
        if let Some(i) = pane.tabs.iter().position(|t| {
            t.kind == TabKind::Editor && t.path.as_ref().map_or(false, |p| p.same_file(&path))
        }) {
            pane.active = i;
            notify_lsp_open(s, &path);
            return;
        }
    }
    // Load into a fresh tab first so a failed open leaves no stray empty tab.
    let mut tab = Tab::untitled(s.next_buf_id);
    match tab.load_file(path.clone()) {
        Ok(()) | Err(LoadError::Remote) => {}
        Err(e) => {
            s.status_msg = Some(e.message(&path));
            return;
        }
    }
    s.next_buf_id += 1;
    let target_tab_idx = {
        let pane = s.panes.get_mut(&ap).unwrap();
        if pane.tab().is_empty_untitled() && !pane.tab().dirty {
            let active = pane.active;
            pane.tabs[active] = tab;
        } else {
            pane.tabs.push(tab);
            pane.active = pane.tabs.len() - 1;
        }
        pane.active
    };
    if path.is_remote() {
        begin_remote_tab_read(s, ap, target_tab_idx, path);
        return;
    }
    notify_lsp_open(s, &path);
}

use vpath::lsp_uri_to_vpath;

fn lsp_root_for_path(s: &State, path: &VPath) -> Option<VPath> {
    if let Some(ex) = &s.explorer {
        if path.strip_prefix(&ex.root).is_some() {
            return Some(ex.root.clone());
        }
    }
    path.parent()
}

// ── LSP position conversion ───────────────────────────────────────────────────
//
// LSP positions count UTF-16 code units within a line (the default encoding; we
// don't negotiate another), while the editor indexes Unicode scalar values. The
// two differ for any character outside the Basic Multilingual Plane (emoji etc.).

/// Char column within `line` for an LSP UTF-16 `character` offset, clamped to the
/// line's length (LSP says an offset past the end means end of line).
fn utf16_to_char_col(text: &Rope, line: usize, utf16: usize) -> usize {
    if line >= text.len_lines() { return 0; }
    let mut units = 0;
    let mut col = 0;
    for ch in text.line(line).chars() {
        if ch == '\n' || ch == '\r' || units >= utf16 { break; }
        units += ch.len_utf16();
        col += 1;
    }
    col
}

/// UTF-16 offset of char column `col` within `line`.
fn char_col_to_utf16(text: &Rope, line: usize, col: usize) -> usize {
    if line >= text.len_lines() { return 0; }
    text.line(line).chars().take(col).map(char::len_utf16).sum()
}

/// Absolute char index for an LSP (line, UTF-16 character) position, clamped to
/// the document (a line past the end means end of document).
fn lsp_position_to_char(text: &Rope, line: usize, character: usize) -> usize {
    if line >= text.len_lines() { return text.len_chars(); }
    text.line_to_char(line) + utf16_to_char_col(text, line, character)
}

/// LSP (line, UTF-16 character) for the editor's char index.
fn char_to_lsp_position(text: &Rope, pos: usize) -> (usize, usize) {
    let pos = pos.min(text.len_chars());
    let line = text.char_to_line(pos);
    (line, char_col_to_utf16(text, line, pos - text.line_to_char(line)))
}

/// Convert LSP TextEdits into (start, end, new_text) char ranges against `text`,
/// keeping array order. Malformed entries are dropped; reversed ranges are fixed.
fn lsp_edits_to_char_ranges(text: &Rope, edits: &[serde_json::Value]) -> Vec<(usize, usize, String)> {
    edits.iter().filter_map(|e| {
        let pos = |p: &serde_json::Value| -> Option<usize> {
            Some(lsp_position_to_char(text, p["line"].as_u64()? as usize, p["character"].as_u64()? as usize))
        };
        let a = pos(&e["range"]["start"])?;
        let b = pos(&e["range"]["end"])?;
        let new_text = e["newText"].as_str()?.to_owned();
        Some((a.min(b), a.max(b), new_text))
    }).collect()
}

/// Apply char-range edits (all relative to the original text) and move cursors
/// along with them. Edits are applied back to front; for edits at the same
/// position the later one goes first so the final text keeps array order, as the
/// LSP spec requires. Overlapping edits (invalid per spec) are skipped.
fn apply_char_edits(text: &mut Rope, cursors: &mut [Cursor], mut edits: Vec<(usize, usize, String)>) -> bool {
    let len = text.len_chars();
    let mut indexed: Vec<(usize, (usize, usize, String))> = edits.drain(..).enumerate().collect();
    // Back to front by start; at equal starts the wider range first (so an insert at
    // the start of a replaced range isn't mistaken for an overlap), then later
    // array entries first.
    indexed.sort_by(|(ia, a), (ib, b)| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(ib.cmp(ia)));
    let mut changed = false;
    let mut floor = usize::MAX; // start of the previously applied (later) edit
    for (_, (start, end, new_text)) in indexed {
        let (start, end) = (start.min(len), end.min(len));
        if end > floor { continue; } // overlaps an edit already applied
        floor = start;
        if start == end && new_text.is_empty() { continue; }
        let new_len = new_text.chars().count();
        text.remove(start..end);
        if !new_text.is_empty() { text.insert(start, &new_text); }
        changed = true;
        for c in cursors.iter_mut() {
            for p in [&mut c.head, &mut c.tail] {
                if *p >= end { *p = *p - (end - start) + new_len; }
                else if *p > start { *p = start + new_len; }
            }
        }
    }
    changed
}

/// Find the editor tab (pane id, tab index) showing `path`.
fn find_tab_for_path(s: &State, path: &VPath) -> Option<(usize, usize)> {
    let mut ids: Vec<usize> = s.panes.keys().copied().collect();
    ids.sort_unstable();
    // Prefer the active pane so edits land in the buffer the user is looking at.
    ids.sort_by_key(|id| *id != s.active_pane);
    ids.into_iter().find_map(|pid| {
        let pane = &s.panes[&pid];
        if pane.kind != PaneKind::Editor { return None; }
        pane.tabs.iter().position(|t| {
            t.kind == TabKind::Editor && t.path.as_ref().map_or(false, |p| p.same_file(path))
        }).map(|ti| (pid, ti))
    })
}

/// Bookkeeping after a buffer was modified outside the normal typing path
/// (LSP edits, formatter reloads, global replace, remote collab ops): copy the new
/// text to split-view siblings, tell the language server, and send the change to
/// collab peers.
fn after_background_edit(s: &mut State, pane_id: usize, tab_idx: usize, before: Option<Rope>) {
    propagate_to_siblings(s, pane_id, tab_idx);
    notify_lsp_change_for(s, pane_id, tab_idx);
    if let Some(before) = before {
        if s.collab.is_some() {
            let buf_id = s.panes[&pane_id].tabs[tab_idx].buf_id;
            notify_collab_change(s); // flush anything pending first
            s.collab_before = Some((buf_id, before));
            notify_collab_change(s);
        }
    }
    s.needs_redraw = true;
}

/// Apply an LSP `TextEdit[]` result to the open buffer for `path`. When
/// `expected_gen` is set the edits were computed for that version of the buffer;
/// if the user has typed since, the (now stale) edits are discarded. Returns true
/// if the buffer changed.
fn apply_text_edits(s: &mut State, path: &VPath, result: &serde_json::Value, expected_gen: Option<u64>) -> bool {
    let Some((pid, ti)) = find_tab_for_path(s, path) else { return false };
    let limit = s.settings.undo_limit;
    let collab = s.collab.is_some();
    let edits = result.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let tab = &mut s.panes.get_mut(&pid).unwrap().tabs[ti];
    if let Some(gen) = expected_gen {
        if tab.edit_generation != gen {
            s.status_msg = Some(format!("{}: edits skipped (document changed while the server was working)",
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
            return false;
        }
    }
    let ranges = lsp_edits_to_char_ranges(&tab.text, edits);
    if ranges.is_empty() { return false; }
    let before = tab.text.clone();
    if !collab { tab.push_undo_snapshot(limit); }
    tab.note_edit();
    let mut cursors = std::mem::take(&mut tab.cursors);
    let changed = apply_char_edits(&mut tab.text, &mut cursors, ranges);
    tab.cursors = cursors;
    tab.clamp_cursors();
    if !changed {
        if !collab { tab.undo_stack.pop(); }
        return false;
    }
    tab.dirty = true;
    after_background_edit(s, pid, ti, Some(before));
    true
}

fn apply_goto_definition(s: &mut State, result: &serde_json::Value, ssh_host: Option<vpath::SshHost>) {
    let loc = if result.is_array() { result.get(0) } else { Some(result) };
    let Some(loc) = loc else { return };
    // Location, or LocationLink (targetUri + targetSelectionRange).
    let uri  = loc["uri"].as_str().or_else(|| loc["targetUri"].as_str()).unwrap_or("");
    let range = if loc["range"].is_object() { &loc["range"] } else { &loc["targetSelectionRange"] };
    let line = range["start"]["line"].as_u64().unwrap_or(0) as usize;
    let col  = range["start"]["character"].as_u64().unwrap_or(0) as usize;
    let Some(path) = lsp_uri_to_vpath(uri, ssh_host.as_ref()) else { return };
    push_cursor_history(s);
    jump_to_lsp_position(s, path, line, col);
}

/// Jump to an LSP position (UTF-16 column) in `path`.
fn jump_to_lsp_position(s: &mut State, path: VPath, line: usize, utf16_col: usize) {
    open_or_reuse_tab(s, path.clone());
    let col = find_tab_for_path(s, &path)
        .map(|(pid, ti)| utf16_to_char_col(&s.panes[&pid].tabs[ti].text, line, utf16_col))
        .unwrap_or(utf16_col);
    jump_to_path_line_col(s, path, line, col);
}

fn apply_references(s: &mut State, result: &serde_json::Value, ssh_host: Option<vpath::SshHost>) {
    let locs = result.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    // Each referenced file is read once (open buffers win over disk).
    let mut texts: HashMap<VPath, Option<Rope>> = HashMap::new();
    let mut results = Vec::new();
    for loc in locs {
        let Some(uri) = loc["uri"].as_str() else { continue };
        let Some(path) = lsp_uri_to_vpath(uri, ssh_host.as_ref()) else { continue };
        let (Some(line), Some(utf16)) = (
            loc["range"]["start"]["line"].as_u64().map(|v| v as usize),
            loc["range"]["start"]["character"].as_u64().map(|v| v as usize),
        ) else { continue };
        let end_utf16 = loc["range"]["end"]["character"].as_u64().map(|v| v as usize).unwrap_or(utf16 + 1);
        let text = texts.entry(path.clone()).or_insert_with(|| {
            find_tab_for_path(s, &path)
                .map(|(pid, ti)| s.panes[&pid].tabs[ti].text.clone())
                .or_else(|| path.as_local_path()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .map(|t| Rope::from_str(&t)))
        });
        let (line_text, col, len) = match text {
            Some(t) if line < t.len_lines() => {
                let col = utf16_to_char_col(t, line, utf16);
                let end = utf16_to_char_col(t, line, end_utf16).max(col + 1);
                let lt: String = t.line(line).chars().take_while(|&c| c != '\n' && c != '\r').take(200).collect();
                (lt, col, end - col)
            }
            _ => (String::new(), utf16, 1),
        };
        results.push(GlobalFindResult { path, line_num: line, line_text, match_col: col, match_len: len });
    }
    s.global_find.results = results;
    s.global_find.selected = 0;
    s.global_find.scroll   = 0;
    s.global_find.focus    = GlobalFindFocus::Results;
    if s.explorer.is_some() {
        // Show the references and let Up/Down/Enter walk them (Escape or typing
        // returns to the editor).
        blur_sidebar(s);
        s.left_view = LeftView::GlobalSearch;
        s.left_panel_visible = true;
        s.sidebar_list_focused = true;
    }
}

/// Pick the organize-imports edits for `path` out of a CodeAction[] result.
/// URIs are compared as decoded paths (servers may encode them differently).
fn organize_imports_edits(path: &VPath, result: &serde_json::Value, ssh_host: Option<&vpath::SshHost>) -> Option<serde_json::Value> {
    let actions = result.as_array()?;
    let action = actions.iter()
        .find(|a| a["kind"].as_str() == Some("source.organizeImports"))
        .or_else(|| actions.first())?;
    let edit = &action["edit"];
    let is_ours = |uri: &str| lsp_uri_to_vpath(uri, ssh_host).map_or(false, |p| p.same_file(path));
    if let Some(changes) = edit["changes"].as_object() {
        if let Some((_, edits)) = changes.iter().find(|(uri, _)| is_ours(uri)) {
            return Some(edits.clone());
        }
    }
    edit["documentChanges"].as_array()?.iter()
        .find(|dc| dc["textDocument"]["uri"].as_str().map_or(false, is_ours))
        .map(|dc| dc["edits"].clone())
}

// ── Cursor position history ───────────────────────────────────────────────────

fn push_cursor_history(s: &mut State) {
    let pid = s.active_pane;
    if !s.panes.get(&pid).map_or(false, |p| p.kind == PaneKind::Editor) { return; }
    if let Some(path) = s.tab().path.clone() {
        let pos = s.tab().primary().head;
        s.cursor_back.push((path, pos));
        s.cursor_fwd.clear();
    }
}

fn cursor_go_back(s: &mut State) {
    let pid = s.active_pane;
    let current = if s.panes.get(&pid).map_or(false, |p| p.kind == PaneKind::Editor) {
        s.tab().path.clone().map(|p| (p, s.tab().primary().head))
    } else { None };
    if let Some(entry) = s.cursor_back.pop() {
        if let Some(cur) = current { s.cursor_fwd.push(cur); }
        let (path, pos) = entry;
        open_or_reuse_tab(s, path);
        let n = s.tab().text.len_chars();
        let clamped = pos.min(n.saturating_sub(1));
        s.tab_mut().cursors = vec![Cursor::new(clamped)];
        s.ensure_visible();
    }
}

fn cursor_go_fwd(s: &mut State) {
    let pid = s.active_pane;
    let current = if s.panes.get(&pid).map_or(false, |p| p.kind == PaneKind::Editor) {
        s.tab().path.clone().map(|p| (p, s.tab().primary().head))
    } else { None };
    if let Some(entry) = s.cursor_fwd.pop() {
        if let Some(cur) = current { s.cursor_back.push(cur); }
        let (path, pos) = entry;
        open_or_reuse_tab(s, path);
        let n = s.tab().text.len_chars();
        let clamped = pos.min(n.saturating_sub(1));
        s.tab_mut().cursors = vec![Cursor::new(clamped)];
        s.ensure_visible();
    }
}

fn term_token_bounds(row: &[terminal::Cell], col: usize) -> (usize, usize) {
    if col >= row.len() || row[col].ch.is_whitespace() { return (col, col); }
    let mut lo = col;
    while lo > 0 && !row[lo - 1].ch.is_whitespace() { lo -= 1; }
    let mut hi = col;
    while hi + 1 < row.len() && !row[hi + 1].ch.is_whitespace() { hi += 1; }
    // Extend hi to include :digits and :digits:digits suffixes (for file:line:col)
    hi = extend_token_with_line_col(row, hi);
    (lo, hi)
}

fn extend_token_with_line_col(row: &[terminal::Cell], mut hi: usize) -> usize {
    for _ in 0..2 {
        if hi + 1 < row.len() && row[hi + 1].ch == ':' {
            let colon_pos = hi + 1;
            let mut end = colon_pos + 1;
            while end < row.len() && row[end].ch.is_ascii_digit() { end += 1; }
            if end > colon_pos + 1 { hi = end - 1; } else { break; }
        } else {
            break;
        }
    }
    hi
}

fn term_word_bounds(row: &[terminal::Cell], col: usize) -> (usize, usize) {
    if col >= row.len() { return (col, col); }
    let ch = row[col].ch;
    if !ch.is_alphanumeric() && ch != '_' { return (col, col); }
    let mut lo = col;
    while lo > 0 && { let c = row[lo - 1].ch; c.is_alphanumeric() || c == '_' } { lo -= 1; }
    let mut hi = col;
    while hi + 1 < row.len() && { let c = row[hi + 1].ch; c.is_alphanumeric() || c == '_' } { hi += 1; }
    (lo, hi)
}

// Get CWD of a running process by PID. Not available on Windows, where the
// terminal relies on the shell reporting it (OSC 7 / OSC 9;9).
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(unused_variables))]
fn term_cwd(pid: Option<u32>) -> Option<std::path::PathBuf> {
    let pid = pid.filter(|&p| p > 0)?;
    #[cfg(target_os = "linux")]
    { std::fs::read_link(format!("/proc/{}/cwd", pid)).ok() }
    #[cfg(target_os = "macos")]
    {
        // Use proc_pidinfo(PROC_PIDVNODEPATHINFO) to get cwd
        // PROC_PIDVNODEPATHINFO = 9, struct proc_vnodepathinfo is 2344 bytes
        // cwd path is at offset of pvi_cdir.vip_path (1024 bytes at offset 152)
        const PROC_PIDVNODEPATHINFO: libc::c_int = 9;
        const STRUCT_SIZE: usize = 2344;
        const CWD_PATH_OFFSET: usize = 152;
        const PATH_MAX: usize = 1024;
        let mut buf = vec![0u8; STRUCT_SIZE];
        let ret = unsafe {
            libc::proc_pidinfo(pid as libc::c_int, PROC_PIDVNODEPATHINFO, 0,
                buf.as_mut_ptr() as *mut libc::c_void, STRUCT_SIZE as libc::c_int)
        };
        if ret <= 0 { return None; }
        let path_bytes = &buf[CWD_PATH_OFFSET..CWD_PATH_OFFSET + PATH_MAX];
        let nul = path_bytes.iter().position(|&b| b == 0).unwrap_or(PATH_MAX);
        if nul == 0 { return None; }
        std::str::from_utf8(&path_bytes[..nul]).ok()
            .map(|s| std::path::PathBuf::from(s))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { None }
}

fn open_token(s: &mut State, token: &str, child_pid: Option<u32>, osc_cwd: Option<std::path::PathBuf>) {
    if token.is_empty() { return; }
    if token.starts_with("http://") || token.starts_with("https://") {
        if let Err(e) = os::open_url(token) {
            s.status_msg = Some(format!("Can't open {token}: {e}"));
        }
        return;
    }
    // Parse optional :line:col suffix
    let (path_part, line, col) = split_path_line_col(token);
    let line = line.unwrap_or(0);
    let col  = col.unwrap_or(0);
    // Expand ~
    let pb = os::expand_home(path_part).unwrap_or_else(|| std::path::PathBuf::from(path_part));
    // Try as absolute path
    if pb.is_absolute() {
        if pb.exists() {
            push_cursor_history(s);
            jump_to_path_line_col(s, VPath::Local(pb), line, col);
        }
        return;
    }
    // Relative path: try osc_cwd first, then proc cwd fallback
    let cwds: Vec<std::path::PathBuf> = [osc_cwd, term_cwd(child_pid)].into_iter().flatten().collect();
    for cwd in cwds {
        let candidate = cwd.join(&pb);
        if candidate.exists() {
            push_cursor_history(s);
            jump_to_path_line_col(s, VPath::Local(candidate), line, col);
            return;
        }
    }
    // Fallback: check if it exists relative to current working dir
    if pb.exists() {
        push_cursor_history(s);
        jump_to_path_line_col(s, VPath::Local(pb), line, col);
    }
}

/// Start an LSP server for `lang`/`ssh_host` if one is not already running.
/// `root` is the workspace root passed to the LSP initialize request.
fn ensure_lsp_server_running(s: &mut State, lang: Lang, ssh_host: Option<vpath::SshHost>, root: Option<VPath>) {
    if s.lsp.has_server_for_lang_host(lang, ssh_host.as_ref()) { return; }

    // Cross-process limit (off by default). Counted per (lang, ssh host) since RAM
    // lives on whichever machine actually runs the server. Absent = unlimited,
    // Some(0) = disabled, Some(n) = at most n.
    let host_tag = ssh_host.as_ref().map(|h| h.display());
    if let Some(lang_key) = lang.as_registry_str() {
        if let Some(&max) = s.settings.lsp_server_limits.get(lang_key) {
            let bin = lsp_binary(lang).unwrap_or("language server");
            if max == 0 {
                s.status_msg = Some(format!("{bin} is disabled (limit 0)."));
                return;
            }
            let active = lsp_registry::count_live(lang_key, host_tag.as_deref());
            if active >= max {
                s.status_msg = Some(format!(
                    "{bin} limit reached ({active}/{max}). Run 'LSP: Manage Servers' to switch."));
                return;
            }
        }
    }

    let op_id = s.next_pane_id;
    s.next_pane_id += 1;
    let proxy = s.proxy.clone();
    let remote_path_dirs = ssh_host.as_ref()
        .map(|host| remote_lsp_path_dirs(&s.settings, host))
        .unwrap_or_default();
    // Per-project launch overrides from `<root>/.vscode/local-text.json` — only if
    // the user trusted exactly this file (it can choose which program runs).
    let overrides = match &root {
        Some(r) => trusted_project_overrides(s, r, lsp_binary(lang).unwrap_or("")),
        None    => lsp::LspLaunchOverrides::default(),
    };
    if let Some(mut srv) = lsp::start_server(lang, op_id, proxy, ssh_host.clone(), remote_path_dirs, root.as_ref(), &overrides) {
        lsp::send_initialize(&mut srv, root.as_ref());
        let bin = lsp_binary(lang).unwrap_or("lsp");
        let invocation = lsp::invocation_string(lang, &overrides)
            .unwrap_or_else(|| lsp_invocation(lang).unwrap_or(bin).to_owned());
        let title = match &ssh_host {
            Some(host) => format!("{} @ {}", bin, host.display()),
            None       => bin.to_owned(),
        };
        let banner = match &ssh_host {
            Some(host) => format!("[{}]$ {}\r\n", host.display(), invocation),
            None       => format!("$ {}\r\n", invocation),
        };
        let mut tp = terminal::new_log_pane(op_id, 120, 40, title);
        let mut banner = banner;
        if let Some(r) = &root {
            banner.push_str(&format!("[local-text] root: {}\r\n", r.display_short()));
            banner.push_str(&format!("[local-text] rootUri: {}\r\n", r.to_lsp_uri()));
        }
        if ssh_host.is_some() {
            banner.push_str("[local-text] remote processId: null\r\n");
        }
        terminal::feed_bytes(&mut tp, banner.as_bytes());
        s.term_panes.insert(op_id, tp);
        let shell_pane = Pane { id: op_id, kind: PaneKind::Terminal, tabs: vec![], term_ids: vec![op_id], active: 0, find: FindBar::new() };
        s.panes.insert(op_id, shell_pane);
        s.lsp.servers.insert(op_id, srv);
        s.lsp_start_errors.remove(&(ssh_host.clone(), lang));
        // Announce to other local-text processes (limit accounting + manage panel).
        if let Some(lang_key) = lang.as_registry_str() {
            lsp_registry::register(op_id, lang_key, host_tag.clone(), root.as_ref().map(|r| r.to_string()));
        }
    } else {
        let scope = ssh_host.as_ref()
            .map(|h| format!("remote {}", h.display()))
            .unwrap_or_else(|| "local machine".to_owned());
        let bin = lsp_binary(lang).unwrap_or("language server");
        let msg = format!("Failed to start {bin} on {scope}");
        s.status_msg = Some(msg.clone());
        s.lsp_start_errors.insert((ssh_host, lang), msg);
    }
}

/// Key under which a project's trusted config is stored in settings.
fn project_trust_key(root: &VPath) -> String { root.to_string() }

/// The project's LSP overrides for `bin`, if its `.vscode/local-text.json` matches
/// the contents the user trusted. An untrusted (or changed) config is ignored and
/// the user is told how to trust it.
fn trusted_project_overrides(s: &mut State, root: &VPath, bin: &str) -> lsp::LspLaunchOverrides {
    let Some(text) = proj_config::read_config_text(root) else { return lsp::LspLaunchOverrides::default() };
    let overrides = proj_config::parse_project_lsp_config(&text, bin);
    if !proj_config::overrides_anything(&overrides) { return overrides; }
    let key = project_trust_key(root);
    if s.settings.trusted_project_configs.get(&key) == Some(&text) {
        return overrides;
    }
    s.status_msg = Some(format!(
        "Ignoring {} (not trusted). Review it, then run 'LSP: Trust Project Settings'.",
        proj_config::CONFIG_REL_PATH,
    ));
    lsp::LspLaunchOverrides::default()
}

/// Trust the current contents of the project's `.vscode/local-text.json` and
/// restart its language servers so the overrides take effect.
fn trust_project_lsp_config(s: &mut State) {
    let (root, _) = current_project_target(s);
    let Some(root) = root else {
        s.status_msg = Some("No project open".to_owned());
        return;
    };
    let Some(text) = proj_config::read_config_text(&root) else {
        s.status_msg = Some(format!("No {} in this project", proj_config::CONFIG_REL_PATH));
        return;
    };
    s.settings.trusted_project_configs.insert(project_trust_key(&root), text);
    s.settings.save();
    // Restart servers for this project so they pick up the overrides.
    let host = root.ssh_host().cloned();
    let ids: Vec<usize> = s.lsp.servers.iter()
        .filter(|(_, srv)| srv.ssh_host == host)
        .map(|(&id, _)| id)
        .collect();
    let mut langs = Vec::new();
    for id in ids {
        if let Some(mut srv) = s.lsp.servers.remove(&id) {
            let _ = srv.process.kill();
            let _ = srv.process.wait();
            lsp_registry::unregister(id);
            langs.push(srv.lang);
        }
    }
    for lang in langs {
        s.lsp_start_errors.remove(&(host.clone(), lang));
        ensure_lsp_server_running(s, lang, host.clone(), Some(root.clone()));
        open_all_docs_for_server(s, lang, host.clone());
    }
    s.status_msg = Some(format!("Trusted {} for {}", proj_config::CONFIG_REL_PATH, root.display_short()));
}

/// Detect project type from indicator files in a local directory and start servers eagerly.
fn start_lsp_for_project(s: &mut State, root: &VPath) {
    let Some(local_root) = root.as_local_path() else { return };
    let indicators: &[(&[&str], Lang)] = &[
        (&["Cargo.toml"],                    Lang::Rust),
        (&["package.json", "tsconfig.json"], Lang::TypeScript),
        (&["pyproject.toml", "setup.py"],    Lang::Python),
    ];
    for &(files, lang) in indicators {
        if files.iter().any(|f| local_root.join(f).exists()) {
            ensure_lsp_server_running(s, lang, None, Some(root.clone()));
        }
    }
}

/// Detect project type from a remote directory listing and start servers eagerly.
fn start_lsp_for_remote_project(s: &mut State, root: &VPath, entries: &[(String, bool)]) {
    let ssh_host = root.ssh_host().cloned();
    let indicators: &[(&[&str], Lang)] = &[
        (&["Cargo.toml"],                    Lang::Rust),
        (&["package.json", "tsconfig.json"], Lang::TypeScript),
        (&["pyproject.toml", "setup.py"],    Lang::Python),
    ];
    for &(files, lang) in indicators {
        if files.iter().any(|f| entries.iter().any(|(name, _)| name == f)) {
            ensure_lsp_server_running(s, lang, ssh_host.clone(), Some(root.clone()));
        }
    }
}

fn notify_lsp_open(s: &mut State, path: &VPath) {
    if let Some((pid, ti)) = find_tab_for_path(s, path) {
        notify_lsp_change_for(s, pid, ti);
    }
}

/// Sync the active editor tab with its language server (after a keyboard event).
fn notify_lsp_change(s: &mut State) {
    let ap = s.active_pane;
    let Some(ti) = s.panes.get(&ap).filter(|p| p.kind == PaneKind::Editor).map(|p| p.active) else { return };
    notify_lsp_change_for(s, ap, ti);
}

/// Send the tab's current text to its language server — didOpen the first time,
/// didChange afterwards — but only if it changed since the last sync (cursor moves
/// and other non-edits send nothing). Starts the server on demand.
fn notify_lsp_change_for(s: &mut State, pane_id: usize, tab_idx: usize) {
    let Some(tab) = s.panes.get(&pane_id).and_then(|p| p.tabs.get(tab_idx)) else { return };
    if tab.kind != TabKind::Editor { return; }
    let Some(path) = tab.path.clone() else { return };
    let lang = tab.lang();
    if lang == Lang::None { return; }
    let gen = tab.edit_generation;
    let server_key = (path.ssh_host().cloned(), lang);
    if s.lsp.server_for_lang_host(lang, path.ssh_host()).is_none()
        && !s.lsp_start_errors.contains_key(&server_key)
    {
        let root = lsp_root_for_path(s, &path);
        ensure_lsp_server_running(s, lang, path.ssh_host().cloned(), root);
    }
    let Some(srv) = s.lsp.server_for_lang_host_mut(lang, path.ssh_host()) else { return };
    if !srv.initialized { return; } // everything is opened once `initialized` is sent
    let tab = &s.panes[&pane_id].tabs[tab_idx];
    if srv.doc_version.contains_key(&path) && tab.lsp_synced_gen == Some(gen) { return; }
    let text = tab.text.to_string();
    if srv.doc_version.contains_key(&path) {
        lsp::notify_did_change(srv, &path, &text);
    } else {
        lsp::notify_did_open(srv, &path, &text);
    }
    if let Some(tab) = s.panes.get_mut(&pane_id).and_then(|p| p.tabs.get_mut(tab_idx)) {
        tab.lsp_synced_gen = Some(gen);
    }
}

/// After a server completes its handshake, open every tab it serves (documents
/// can't be sent before `initialized`).
fn open_all_docs_for_server(s: &mut State, lang: Lang, host: Option<vpath::SshHost>) {
    let targets: Vec<(usize, usize)> = s.panes.iter()
        .filter(|(_, p)| p.kind == PaneKind::Editor)
        .flat_map(|(&pid, p)| p.tabs.iter().enumerate().map(move |(ti, t)| (pid, ti, t)))
        .filter(|(_, _, t)| t.kind == TabKind::Editor
            && t.lang() == lang
            && t.path.as_ref().map_or(false, |p| p.ssh_host() == host.as_ref()))
        .map(|(pid, ti, _)| (pid, ti))
        .collect();
    let mut seen: HashSet<VPath> = HashSet::new();
    for (pid, ti) in targets {
        let path = s.panes[&pid].tabs[ti].path.clone();
        if let Some(path) = path {
            if !seen.insert(path) { continue; } // one didOpen per file (split views share it)
        }
        if let Some(t) = s.panes.get_mut(&pid).and_then(|p| p.tabs.get_mut(ti)) { t.lsp_synced_gen = None; }
        notify_lsp_change_for(s, pid, ti);
    }
}

/// Copy a tab's buffer to every other tab sharing its `buf_id` (split views),
/// invalidating their highlighting and keeping their cursors in range.
fn propagate_to_siblings(s: &mut State, pane_id: usize, tab_idx: usize) {
    let Some(src) = s.panes.get(&pane_id).and_then(|p| p.tabs.get(tab_idx)) else { return };
    let (buf_id, text, path, dirty, gen) = (src.buf_id, src.text.clone(), src.path.clone(), src.dirty, src.edit_generation);
    for (&pid, p) in s.panes.iter_mut() {
        for (ti, t) in p.tabs.iter_mut().enumerate() {
            if t.buf_id != buf_id || (pid == pane_id && ti == tab_idx) || t.kind != TabKind::Editor { continue; }
            t.note_edit();
            t.text  = text.clone();
            t.path  = path.clone();
            t.dirty = dirty;
            t.lsp_synced_gen = Some(t.edit_generation); // same content the server already has
            t.clamp_cursors();
        }
    }
    if pane_id == s.active_pane && s.panes.get(&pane_id).map_or(false, |p| p.active == tab_idx) {
        s.last_sync_pane = pane_id;
        s.last_sync_tab  = tab_idx;
        s.last_sync_gen  = gen;
    }
}

/// Get the username from the environment for the collab peer name.
fn whoami_or_default() -> String {
    os::user_name().unwrap_or_else(|| "Guest".to_owned())
}

/// `\` → `/` on Windows (path display strings), unchanged elsewhere.
fn to_slashes(s: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(windows) && s.contains('\\') { s.replace('\\', "/").into() } else { s.into() }
}

/// The last component of a path string (either separator on Windows).
fn path_basename(s: &str) -> &str {
    s.rsplit(|c| c == '/' || (cfg!(windows) && c == '\\')).next().unwrap_or(s)
}

// ── Syntax highlighter ────────────────────────────────────────────────────────

fn is_keyword(word: &str, lang: Lang) -> bool {
    match lang {
        Lang::Rust => matches!(word,
            "as"|"async"|"await"|"break"|"const"|"continue"|"crate"|"dyn"|"else"|
            "enum"|"extern"|"false"|"fn"|"for"|"if"|"impl"|"in"|"let"|"loop"|
            "match"|"mod"|"move"|"mut"|"pub"|"ref"|"return"|"self"|"Self"|
            "static"|"struct"|"super"|"trait"|"true"|"type"|"union"|"unsafe"|
            "use"|"where"|"while"
        ),
        Lang::Python => matches!(word,
            "and"|"as"|"assert"|"async"|"await"|"break"|"class"|"continue"|
            "def"|"del"|"elif"|"else"|"except"|"False"|"finally"|"for"|"from"|
            "global"|"if"|"import"|"in"|"is"|"lambda"|"None"|"nonlocal"|"not"|
            "or"|"pass"|"raise"|"return"|"True"|"try"|"while"|"with"|"yield"
        ),
        Lang::TypeScript => matches!(word,
            "abstract"|"as"|"async"|"await"|"break"|"case"|"catch"|"class"|
            "const"|"continue"|"debugger"|"declare"|"default"|"delete"|"do"|
            "else"|"enum"|"export"|"extends"|"false"|"finally"|"for"|"from"|
            "function"|"if"|"implements"|"import"|"in"|"infer"|"instanceof"|
            "interface"|"is"|"keyof"|"let"|"namespace"|"new"|"null"|"of"|
            "package"|"private"|"protected"|"public"|"readonly"|"return"|
            "satisfies"|"static"|"super"|"switch"|"this"|"throw"|"true"|"try"|
            "type"|"typeof"|"undefined"|"var"|"void"|"while"|"with"|"yield"
        ),
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown | Lang::Css | Lang::Html => false,
    }
}

fn is_type_kw(word: &str, lang: Lang) -> bool {
    match lang {
        Lang::Rust => matches!(word,
            "bool"|"char"|"f32"|"f64"|"i8"|"i16"|"i32"|"i64"|"i128"|"isize"|
            "str"|"u8"|"u16"|"u32"|"u64"|"u128"|"usize"|"String"|"Vec"|
            "Option"|"Result"|"Box"|"Arc"|"Rc"|"HashMap"|"HashSet"
        ),
        Lang::Python => matches!(word,
            "bool"|"bytes"|"bytearray"|"complex"|"dict"|"float"|"frozenset"|
            "int"|"list"|"memoryview"|"object"|"range"|"set"|"str"|"tuple"|"type"
        ),
        Lang::TypeScript => matches!(word,
            "boolean"|"bigint"|"never"|"number"|"string"|"symbol"|"unknown"
        ),
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown | Lang::Css | Lang::Html => false,
    }
}

fn classify_word(word: &str, lang: Lang, is_call: bool) -> u32 {
    if is_keyword(word, lang)  { return HL_KEYWORD; }
    if is_type_kw(word, lang)  { return HL_TYPE; }
    if word.chars().next().map_or(false, |c| c.is_uppercase()) { return HL_TYPE; }
    if is_call                 { return HL_FUNC; }
    FG
}

fn strip_jsonc_comments(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    while i < len {
        if in_str {
            out.push(chars[i]);
            if chars[i] == '\\' && i + 1 < len {
                i += 1;
                out.push(chars[i]);
            } else if chars[i] == '"' {
                in_str = false;
            }
            i += 1;
        } else if chars[i] == '"' {
            in_str = true;
            out.push(chars[i]);
            i += 1;
        } else if i + 1 < len && chars[i] == '/' && chars[i + 1] == '/' {
            while i < len && chars[i] != '\n' { i += 1; }
        } else if i + 1 < len && chars[i] == '/' && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '/') { i += 1; }
            if i + 1 < len { i += 2; }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Remove trailing commas (`,` followed only by whitespace before `}` / `]`) from
/// comment-free JSONC so it can be validated with a strict JSON parser.
fn strip_trailing_commas(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut in_str = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() { i += 1; out.push(chars[i]); }
            else if c == '"' { in_str = false; }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|ch| !ch.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) { out.push(c); }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

#[derive(Debug, PartialEq)]
enum JsonTok {
    Open(char),
    Close(char),
    Comma,
    Colon,
    /// String, number or literal — copied verbatim (no re-encoding of numbers).
    Atom(String),
    /// A `//` or `/* */` comment; `own_line` = it started a line in the source.
    Comment { text: String, line: bool, own_line: bool },
}

fn tokenize_json(src: &str) -> Option<Vec<JsonTok>> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut newline_since_tok = true;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if c == '\n' { newline_since_tok = true; }
            i += 1;
            continue;
        }
        let own_line = newline_since_tok;
        newline_since_tok = false;
        match c {
            '{' | '[' => { toks.push(JsonTok::Open(c)); i += 1; }
            '}' | ']' => { toks.push(JsonTok::Close(c)); i += 1; }
            ',' => { toks.push(JsonTok::Comma); i += 1; }
            ':' => { toks.push(JsonTok::Colon); i += 1; }
            '"' => {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' { i += 1; }
                    i += 1;
                }
                if i >= chars.len() { return None; } // unterminated string
                i += 1;
                toks.push(JsonTok::Atom(chars[start..i].iter().collect()));
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                let start = i;
                while i < chars.len() && chars[i] != '\n' { i += 1; }
                let text: String = chars[start..i].iter().collect();
                toks.push(JsonTok::Comment { text: text.trim_end().to_owned(), line: true, own_line });
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                let start = i;
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') { i += 1; }
                if i + 1 >= chars.len() { return None; } // unterminated comment
                i += 2;
                toks.push(JsonTok::Comment { text: chars[start..i].iter().collect(), line: false, own_line });
            }
            _ => {
                let start = i;
                while i < chars.len() && !chars[i].is_whitespace()
                    && !matches!(chars[i], '{' | '}' | '[' | ']' | ',' | ':' | '"')
                    && !(chars[i] == '/' && matches!(chars.get(i + 1), Some('/') | Some('*')))
                {
                    i += 1;
                }
                toks.push(JsonTok::Atom(chars[start..i].iter().collect()));
            }
        }
    }
    Some(toks)
}

/// Pretty-print JSON / JSONC with 2-space indentation. Comments, trailing commas,
/// key order and number literals are kept exactly as written. Returns None if the
/// document isn't valid (it is left untouched then).
fn format_json_text(src: &str, jsonc: bool) -> Option<String> {
    let check = if jsonc { strip_trailing_commas(&strip_jsonc_comments(src)) } else { src.to_owned() };
    serde_json::from_str::<serde::de::IgnoredAny>(&check).ok()?;
    let toks = tokenize_json(src)?;
    let mut out = String::with_capacity(src.len() + src.len() / 4);
    let mut level: usize = 0;
    let mut pending_newline = false;
    let newline = |out: &mut String, level: usize| {
        while out.ends_with(' ') { out.pop(); }
        out.push('\n');
        for _ in 0..level { out.push_str("  "); }
    };
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            JsonTok::Open(c) => {
                if pending_newline { newline(&mut out, level); pending_newline = false; }
                let close = if *c == '{' { '}' } else { ']' };
                if toks.get(i + 1) == Some(&JsonTok::Close(close)) {
                    out.push(*c);
                    out.push(close);
                    i += 2;
                    continue;
                }
                out.push(*c);
                level += 1;
                pending_newline = true;
            }
            JsonTok::Close(c) => {
                level = level.saturating_sub(1);
                newline(&mut out, level);
                pending_newline = false;
                out.push(*c);
            }
            JsonTok::Comma => { out.push(','); pending_newline = true; }
            JsonTok::Colon => out.push_str(": "),
            JsonTok::Atom(a) => {
                if pending_newline { newline(&mut out, level); pending_newline = false; }
                out.push_str(a);
            }
            JsonTok::Comment { text, line, own_line } => {
                if *own_line && !out.is_empty() {
                    newline(&mut out, level);
                } else if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push_str(text);
                // A line comment always ends its line; an own-line block comment too.
                pending_newline = *line || *own_line || pending_newline;
            }
        }
        i += 1;
    }
    out.push('\n');
    Some(out)
}

fn format_json_document(s: &mut State) {
    if active_editor_tab(s).is_none() { return; }
    let lang = s.tab().lang();
    let text = s.tab().text.to_string();
    let Some(pretty) = format_json_text(&text, lang == Lang::Jsonc) else {
        s.status_msg = Some("Format: document is not valid JSON; left unchanged".to_owned());
        return;
    };
    if pretty == text { return; }
    s.push_undo(false);
    let t = s.tab_mut();
    t.text = Rope::from_str(&pretty);
    t.clamp_cursors();
    t.dirty = true;
}

fn highlight_json_line(chars: &[char], jsonc: bool, mut state: MlState, rainbow: bool, mut bracket_depth: i32) -> (Vec<u32>, MlState, i32) {
    let len = chars.len();
    let mut out = vec![FG; len];
    let mut i = 0;
    macro_rules! fill { ($from:expr, $to:expr, $color:expr) => { for k in $from..($to).min(len) { out[k] = $color; } }; }
    while i < len {
        match state {
            MlState::BlockComment => {
                if i + 1 < len && chars[i] == '*' && chars[i + 1] == '/' {
                    fill!(i, i + 2, HL_COMMENT);
                    i += 2;
                    state = MlState::Normal;
                } else {
                    out[i] = HL_COMMENT;
                    i += 1;
                }
            }
            _ => {
                // JSONC line comment
                if jsonc && i + 1 < len && chars[i] == '/' && chars[i + 1] == '/' {
                    fill!(i, len, HL_COMMENT);
                    i = len;
                    continue;
                }
                // JSONC block comment
                if jsonc && i + 1 < len && chars[i] == '/' && chars[i + 1] == '*' {
                    fill!(i, i + 2, HL_COMMENT);
                    i += 2;
                    state = MlState::BlockComment;
                    continue;
                }
                // String — color as key (HL_FUNC) if followed by ':', else value (HL_STRING)
                if chars[i] == '"' {
                    let start = i;
                    i += 1;
                    while i < len {
                        if chars[i] == '\\' && i + 1 < len { i += 2; }
                        else if chars[i] == '"' { i += 1; break; }
                        else { i += 1; }
                    }
                    let mut j = i;
                    while j < len && (chars[j] == ' ' || chars[j] == '\t') { j += 1; }
                    let color = if j < len && chars[j] == ':' { HL_FUNC } else { HL_STRING };
                    fill!(start, i, color);
                    continue;
                }
                // Number (including negative and scientific notation)
                if chars[i].is_ascii_digit() || (chars[i] == '-' && i + 1 < len && chars[i + 1].is_ascii_digit()) {
                    let start = i;
                    while i < len && (chars[i].is_ascii_digit() || matches!(chars[i], '.' | 'e' | 'E' | '+' | '-')) {
                        i += 1;
                    }
                    fill!(start, i, HL_NUMBER);
                    continue;
                }
                // Keywords: true, false, null
                if chars[i].is_ascii_alphabetic() {
                    let start = i;
                    while i < len && chars[i].is_ascii_alphabetic() { i += 1; }
                    let word: String = chars[start..i].iter().collect();
                    if matches!(word.as_str(), "true" | "false" | "null") {
                        fill!(start, i, HL_KEYWORD);
                    }
                    continue;
                }
                // Structural punctuation: dim it
                if matches!(chars[i], ':' | ',' | '{' | '}' | '[' | ']') {
                    if rainbow && matches!(chars[i], '[' | '{') {
                        out[i] = RAINBOW[(bracket_depth.max(0) as usize) % RAINBOW.len()];
                        bracket_depth += 1;
                    } else if rainbow && matches!(chars[i], ']' | '}') {
                        bracket_depth = bracket_depth.saturating_sub(1);
                        out[i] = RAINBOW[(bracket_depth.max(0) as usize) % RAINBOW.len()];
                    } else {
                        out[i] = FG_DIM;
                    }
                    i += 1;
                    continue;
                }
                i += 1;
            }
        }
    }
    (out, state, bracket_depth)
}

fn highlight_markdown_line(chars: &[char], state: MlState, _rainbow: bool, bracket_depth: i32) -> (Vec<u32>, MlState, i32) {
    let len = chars.len();
    let mut out = vec![FG; len];
    macro_rules! fill_range { ($from:expr, $to:expr, $color:expr) => { for k in $from..($to).min(len) { out[k] = $color; } }; }

    // Inside a fenced code block
    if state == MlState::CodeFence {
        let is_fence = len >= 3 && chars[0] == '`' && chars[1] == '`' && chars[2] == '`';
        if is_fence {
            fill_range!(0, len, HL_COMMENT);
            return (out, MlState::Normal, bracket_depth);
        }
        fill_range!(0, len, HL_NUMBER);
        return (out, MlState::CodeFence, bracket_depth);
    }

    // Skip pure whitespace lines
    if chars.iter().all(|c| c.is_whitespace()) {
        return (out, MlState::Normal, bracket_depth);
    }

    // ATX heading: 1-6 '#' followed by space or end
    let heading_hashes = chars.iter().take_while(|&&c| c == '#').count();
    if heading_hashes >= 1 && heading_hashes <= 6
        && (len == heading_hashes || chars[heading_hashes] == ' ')
    {
        fill_range!(0, len, HL_FUNC);
        return (out, MlState::Normal, bracket_depth);
    }

    // Fenced code block opening: line starts with ```
    if len >= 3 && chars[0] == '`' && chars[1] == '`' && chars[2] == '`' {
        fill_range!(0, len, HL_COMMENT);
        return (out, MlState::CodeFence, bracket_depth);
    }

    // Blockquote: starts with '>'
    if chars[0] == '>' {
        fill_range!(0, len, HL_COMMENT);
        return (out, MlState::Normal, bracket_depth);
    }

    // Horizontal rule: 3+ repeated '-', '*', or '_' (possibly with spaces), nothing else
    {
        let trimmed: Vec<char> = chars.iter().filter(|&&c| !c.is_whitespace()).cloned().collect();
        if trimmed.len() >= 3 && trimmed.iter().all(|&c| c == trimmed[0]) && matches!(trimmed[0], '-' | '*' | '_') {
            fill_range!(0, len, FG_DIM);
            return (out, MlState::Normal, bracket_depth);
        }
    }

    // List marker: starts with '- ', '* ', '+ ', or '<digits>. '
    {
        let mut li = 0;
        while li < len && chars[li] == ' ' { li += 1; }  // leading indent
        let marker_start = li;
        let is_unordered = li < len && matches!(chars[li], '-' | '*' | '+') && li + 1 < len && chars[li + 1] == ' ';
        let mut is_ordered = false;
        if !is_unordered && li < len && chars[li].is_ascii_digit() {
            let num_start = li;
            while li < len && chars[li].is_ascii_digit() { li += 1; }
            if li < len && chars[li] == '.' && li + 1 < len && chars[li + 1] == ' ' {
                is_ordered = true;
                li += 1; // include the '.'
            } else {
                li = num_start; // reset
            }
        }
        if is_unordered || is_ordered {
            let marker_end = if is_unordered { marker_start + 1 } else { li };
            fill_range!(marker_start, marker_end + 1, FG_DIM);
            // rest gets inline scan below — but for simplicity just color rest FG (already set)
            // We still need to scan the rest for inline markup, so don't return here
        }
    }

    // Inline scan
    let mut i = 0;
    while i < len {
        // HTML comment <!-- ... -->
        if i + 3 < len && chars[i] == '<' && chars[i+1] == '!' && chars[i+2] == '-' && chars[i+3] == '-' {
            let start = i;
            i += 4;
            while i + 2 < len && !(chars[i] == '-' && chars[i+1] == '-' && chars[i+2] == '>') { i += 1; }
            let end = (i + 3).min(len);
            fill_range!(start, end, HL_COMMENT);
            i = end;
            continue;
        }
        // Inline code: `...`
        if chars[i] == '`' {
            let start = i;
            i += 1;
            while i < len && chars[i] != '`' { i += 1; }
            let end = (i + 1).min(len);
            fill_range!(start, end, HL_STRING);
            i = end;
            continue;
        }
        // Image: ![alt](url)
        if chars[i] == '!' && i + 1 < len && chars[i+1] == '[' {
            out[i] = FG_DIM;
            i += 1;
            let start = i;
            i += 1;
            while i < len && chars[i] != ']' { i += 1; }
            fill_range!(start, i + 1, HL_FUNC);
            i = (i + 1).min(len);
            if i < len && chars[i] == '(' {
                let url_open = i;
                i += 1;
                while i < len && chars[i] != ')' { i += 1; }
                out[url_open] = FG_DIM;
                fill_range!(url_open + 1, i, HL_COMMENT);
                if i < len { out[i] = FG_DIM; i += 1; }
            }
            continue;
        }
        // Link: [text](url)
        if chars[i] == '[' {
            let start = i;
            i += 1;
            while i < len && chars[i] != ']' { i += 1; }
            fill_range!(start, i + 1, HL_FUNC);
            i = (i + 1).min(len);
            if i < len && chars[i] == '(' {
                let url_open = i;
                i += 1;
                while i < len && chars[i] != ')' { i += 1; }
                out[url_open] = FG_DIM;
                fill_range!(url_open + 1, i, HL_COMMENT);
                if i < len { out[i] = FG_DIM; i += 1; }
            }
            continue;
        }
        // Bold+italic: ***text*** or ___text___
        if i + 2 < len && ((chars[i] == '*' && chars[i+1] == '*' && chars[i+2] == '*')
                        || (chars[i] == '_' && chars[i+1] == '_' && chars[i+2] == '_'))
        {
            let delim = chars[i];
            let start = i;
            i += 3;
            while i + 2 < len && !(chars[i] == delim && chars[i+1] == delim && chars[i+2] == delim) { i += 1; }
            let end = (i + 3).min(len);
            fill_range!(start, end, HL_FUNC);
            i = end;
            continue;
        }
        // Bold: **text** or __text__
        if i + 1 < len && ((chars[i] == '*' && chars[i+1] == '*') || (chars[i] == '_' && chars[i+1] == '_')) {
            let delim = chars[i];
            let start = i;
            i += 2;
            while i + 1 < len && !(chars[i] == delim && chars[i+1] == delim) { i += 1; }
            let end = (i + 2).min(len);
            fill_range!(start, end, HL_TYPE);
            i = end;
            continue;
        }
        // Italic: *text* (guard: preceded by whitespace or line start)
        if chars[i] == '*' && i + 1 < len && chars[i+1] != '*' && chars[i+1] != ' '
            && (i == 0 || chars[i-1].is_whitespace())
        {
            let start = i;
            i += 1;
            while i < len && chars[i] != '*' { i += 1; }
            let end = (i + 1).min(len);
            fill_range!(start, end, HL_STRING);
            i = end;
            continue;
        }
        // Italic: _text_ (guard: preceded by whitespace or line start, not inside word)
        if chars[i] == '_' && i + 1 < len && chars[i+1] != '_' && chars[i+1] != ' '
            && (i == 0 || chars[i-1].is_whitespace())
        {
            let start = i;
            i += 1;
            while i < len && chars[i] != '_' { i += 1; }
            let end = (i + 1).min(len);
            fill_range!(start, end, HL_STRING);
            i = end;
            continue;
        }
        i += 1;
    }

    (out, MlState::Normal, bracket_depth)
}

fn render_markdown_to_lines(text: &ropey::Rope) -> Vec<(String, Vec<u32>)> {
    let mut state = MlState::Normal;
    (0..text.len_lines()).map(|li| {
        let chars: Vec<char> = text.line(li)
            .chars().take_while(|&c| c != '\n' && c != '\r').collect();
        let line_str: String = chars.iter().collect();
        let (colors, ns, _) = highlight_markdown_line(&chars, state, false, 0);
        state = ns;
        (line_str, colors)
    }).collect()
}

// ── CSS / Less / SCSS highlighter ────────────────────────────────────────────
fn highlight_css_line(chars: &[char], mut state: MlState, mut bracket_depth: i32) -> (Vec<u32>, MlState, i32) {
    let len = chars.len();
    let mut out = vec![FG; len];
    let mut i = 0;

    macro_rules! fill {
        ($from:expr, $to:expr, $color:expr) => {
            for k in $from..($to).min(len) { out[k] = $color; }
        };
    }

    // Continue a block comment carried from the previous line
    if state == MlState::BlockComment {
        while i < len {
            out[i] = HL_COMMENT;
            if chars[i] == '*' && i + 1 < len && chars[i+1] == '/' {
                out[i+1] = HL_COMMENT; i += 2;
                state = MlState::Normal;
                break;
            }
            i += 1;
        }
    }

    while i < len {
        let ch = chars[i];

        // /* ... */ block comment
        if ch == '/' && i + 1 < len && chars[i+1] == '*' {
            fill!(i, i + 2, HL_COMMENT); i += 2;
            state = MlState::BlockComment;
            while i < len {
                out[i] = HL_COMMENT;
                if chars[i] == '*' && i + 1 < len && chars[i+1] == '/' {
                    out[i+1] = HL_COMMENT; i += 2;
                    state = MlState::Normal;
                    break;
                }
                i += 1;
            }
            continue;
        }

        // // line comment (Less / SCSS)
        if ch == '/' && i + 1 < len && chars[i+1] == '/' {
            fill!(i, len, HL_COMMENT);
            break;
        }

        // String literals
        if ch == '"' || ch == '\'' {
            let q = ch;
            out[i] = HL_STRING; i += 1;
            while i < len {
                out[i] = HL_STRING;
                if chars[i] == '\\' && i + 1 < len { out[i+1] = HL_STRING; i += 2; }
                else if chars[i] == q { i += 1; break; }
                else { i += 1; }
            }
            continue;
        }

        // @rule or @var (Less) — @media, @import, @keyframes, @font-face, etc.
        if ch == '@' {
            let start = i;
            i += 1;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
                i += 1;
            }
            fill!(start, i, HL_KEYWORD);
            continue;
        }

        // $var — SCSS variable
        if ch == '$' {
            let start = i;
            i += 1;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
                i += 1;
            }
            fill!(start, i, HL_TYPE);
            continue;
        }

        // #hex color or #id selector
        if ch == '#' {
            let start = i;
            i += 1;
            // If next chars are hex digits only and 3 or 6 long → color literal
            let hex_start = i;
            while i < len && chars[i].is_ascii_alphanumeric() { i += 1; }
            let word_len = i - hex_start;
            let all_hex = chars[hex_start..i].iter().all(|c| c.is_ascii_hexdigit());
            if all_hex && (word_len == 3 || word_len == 4 || word_len == 6 || word_len == 8) {
                fill!(start, i, HL_NUMBER); // color literal
            } else {
                fill!(start, i, HL_FUNC);  // #id selector
            }
            continue;
        }

        // { / } track depth
        if ch == '{' { bracket_depth += 1; i += 1; continue; }
        if ch == '}' { bracket_depth = bracket_depth.saturating_sub(1); i += 1; continue; }

        // !important
        if ch == '!' {
            let start = i;
            i += 1;
            while i < len && chars[i].is_alphabetic() { i += 1; }
            fill!(start, i, HL_KEYWORD);
            continue;
        }

        // . class selector or : / :: pseudo-class/element
        if ch == '.' || ch == ':' {
            let color = if ch == ':' { HL_TYPE } else { HL_FUNC };
            out[i] = color; i += 1;
            if ch == ':' && i < len && chars[i] == ':' { out[i] = HL_TYPE; i += 1; } // ::pseudo
            let _start = i;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
                out[i] = color; i += 1;
            }
            continue;
        }

        // [attribute] selector
        if ch == '[' {
            let start = i;
            i += 1;
            while i < len && chars[i] != ']' { i += 1; }
            if i < len { i += 1; }
            fill!(start, i, HL_TYPE);
            continue;
        }

        // CSS custom property (--var)
        if ch == '-' && i + 1 < len && chars[i+1] == '-' {
            let start = i;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
                i += 1;
            }
            fill!(start, i, HL_TYPE);
            continue;
        }

        // Number with optional unit
        if ch.is_ascii_digit() || (ch == '.' && i + 1 < len && chars[i+1].is_ascii_digit()) {
            let start = i;
            while i < len && (chars[i].is_ascii_digit() || chars[i] == '.') { i += 1; }
            fill!(start, i, HL_NUMBER);
            // unit immediately following
            let unit_start = i;
            while i < len && chars[i].is_alphabetic() { i += 1; }
            if i > unit_start { fill!(unit_start, i, HL_TYPE); }
            if i < len && chars[i] == '%' { out[i] = HL_TYPE; i += 1; }
            continue;
        }

        // Identifier: property name (inside {}) or element selector / value keyword (outside)
        if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();

            // Check what follows (skip whitespace) to determine property vs value
            let mut j = i;
            while j < len && chars[j] == ' ' { j += 1; }
            let followed_by_colon = j < len && chars[j] == ':' && j + 1 < len && chars[j+1] != ':';

            let color = if bracket_depth > 0 && followed_by_colon {
                HL_FUNC // CSS property name
            } else if bracket_depth > 0 {
                // Value keyword or unit
                match word.as_str() {
                    "none" | "auto" | "inherit" | "initial" | "unset" | "revert" |
                    "normal" | "bold" | "italic" | "oblique" | "bolder" | "lighter" |
                    "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" |
                    "inset" | "outset" | "hidden" | "visible" | "collapse" |
                    "block" | "inline" | "flex" | "grid" | "contents" | "flow" |
                    "table" | "list-item" | "run-in" |
                    "absolute" | "relative" | "fixed" | "sticky" | "static" |
                    "center" | "left" | "right" | "top" | "bottom" | "middle" |
                    "start" | "end" | "stretch" | "baseline" |
                    "transparent" | "currentColor" | "currentcolor" |
                    "serif" | "sans-serif" | "monospace" | "cursive" | "fantasy" |
                    "pointer" | "default" | "crosshair" | "move" | "not-allowed" |
                    "nowrap" | "wrap" | "pre" | "clip" | "ellipsis" | "break-word" |
                    "uppercase" | "lowercase" | "capitalize" |
                    "ltr" | "rtl" | "both" | "forwards" | "backwards" | "infinite" |
                    "ease" | "linear" | "ease-in" | "ease-out" | "ease-in-out" |
                    "alternate" | "reverse" | "paused" | "running" | "fill" | "stroke" |
                    // common color names
                    "black" | "white" | "red" | "green" | "blue" | "yellow" |
                    "orange" | "purple" | "pink" | "gray" | "grey" | "cyan" | "magenta"
                        => HL_KEYWORD,
                    // units (standalone without preceding number, e.g. in calc())
                    "px" | "em" | "rem" | "vh" | "vw" | "vmin" | "vmax" |
                    "pt" | "cm" | "mm" | "in" | "ex" | "ch" | "fr" |
                    "deg" | "rad" | "turn" | "ms" | "s" | "dpi" | "dpcm" | "dppx"
                        => HL_TYPE,
                    _ => FG
                }
            } else {
                FG // element name in selector — keep as foreground
            };
            fill!(start, i, color);
            continue;
        }

        i += 1;
    }

    (out, state, bracket_depth)
}

// ── HTML / SVG / XML highlighter ─────────────────────────────────────────────
fn highlight_html_line(chars: &[char], mut state: MlState) -> (Vec<u32>, MlState, i32) {
    let len = chars.len();
    let mut out = vec![FG; len];
    let mut i = 0;

    macro_rules! fill {
        ($from:expr, $to:expr, $color:expr) => {
            for k in $from..($to).min(len) { out[k] = $color; }
        };
    }

    while i < len {
        match state {
            MlState::HtmlComment => {
                // Scan for -->
                if i + 2 < len && chars[i] == '-' && chars[i+1] == '-' && chars[i+2] == '>' {
                    fill!(i, i + 3, HL_COMMENT); i += 3;
                    state = MlState::Normal;
                } else {
                    out[i] = HL_COMMENT; i += 1;
                }
            }
            MlState::HtmlTag => {
                // Inside a tag: highlight attributes and values, exit on > or />
                if chars[i] == '>' {
                    i += 1; state = MlState::Normal;
                } else if chars[i] == '/' && i + 1 < len && chars[i+1] == '>' {
                    fill!(i, i + 2, FG); i += 2; state = MlState::Normal;
                } else if chars[i] == '"' || chars[i] == '\'' {
                    let q = chars[i];
                    out[i] = HL_STRING; i += 1;
                    while i < len {
                        out[i] = HL_STRING;
                        if chars[i] == '\\' && i + 1 < len { out[i+1] = HL_STRING; i += 2; }
                        else if chars[i] == q { i += 1; break; }
                        else { i += 1; }
                    }
                } else if chars[i].is_alphabetic() || chars[i] == '_' || chars[i] == '-'
                    || chars[i] == ':' // namespace prefix (xml:lang etc.)
                {
                    let start = i;
                    while i < len && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '-' | ':' | '.')) {
                        i += 1;
                    }
                    fill!(start, i, HL_TYPE); // attribute name
                } else {
                    i += 1;
                }
            }
            _ => {
                // Normal / any other state — parse HTML content
                // <!-- comment -->
                if i + 3 < len && chars[i] == '<' && chars[i+1] == '!' && chars[i+2] == '-' && chars[i+3] == '-' {
                    fill!(i, i + 4, HL_COMMENT); i += 4;
                    state = MlState::HtmlComment;
                    continue;
                }
                // <! DOCTYPE etc.
                if chars[i] == '<' && i + 1 < len && chars[i+1] == '!' {
                    let start = i;
                    i += 2;
                    while i < len && chars[i] != '>' { i += 1; }
                    if i < len { i += 1; }
                    fill!(start, i, HL_KEYWORD);
                    continue;
                }
                // </closing-tag>
                if chars[i] == '<' && i + 1 < len && chars[i+1] == '/' {
                    out[i] = FG; out[i+1] = FG; i += 2;
                    let _start = i;
                    while i < len && (chars[i].is_alphanumeric() || matches!(chars[i], '-' | '_' | ':' | '.')) {
                        out[i] = HL_FUNC; i += 1;
                    }
                    // consume rest up to >
                    while i < len && chars[i] != '>' { i += 1; }
                    if i < len { i += 1; }
                    continue;
                }
                // <opening-tag
                if chars[i] == '<' && i + 1 < len && chars[i+1].is_alphabetic() {
                    out[i] = FG; i += 1;
                    let _start = i;
                    while i < len && (chars[i].is_alphanumeric() || matches!(chars[i], '-' | '_' | ':' | '.')) {
                        out[i] = HL_FUNC; i += 1;
                    }
                    state = MlState::HtmlTag;
                    continue;
                }
                // &entity; references
                if chars[i] == '&' {
                    let start = i;
                    i += 1;
                    while i < len && chars[i] != ';' && chars[i] != ' ' && i - start < 12 { i += 1; }
                    if i < len && chars[i] == ';' { i += 1; fill!(start, i, HL_KEYWORD); }
                    else { fill!(start, i, FG); } // not a valid entity, leave as-is
                    continue;
                }
                i += 1;
            }
        }
    }

    (out, state, 0)
}

fn highlight_line(chars: &[char], lang: Lang, mut state: MlState, rainbow: bool, mut bracket_depth: i32) -> (Vec<u32>, MlState, i32) {
    if matches!(lang, Lang::Json | Lang::Jsonc) {
        return highlight_json_line(chars, lang == Lang::Jsonc, state, rainbow, bracket_depth);
    }
    if lang == Lang::Markdown {
        return highlight_markdown_line(chars, state, rainbow, bracket_depth);
    }
    if lang == Lang::Css {
        return highlight_css_line(chars, state, bracket_depth);
    }
    if lang == Lang::Html {
        return highlight_html_line(chars, state);
    }
    let len = chars.len();
    let mut out = vec![FG; len];
    let mut i = 0;

    macro_rules! fill {
        ($from:expr, $to:expr, $color:expr) => {
            for k in $from..($to).min(len) { out[k] = $color; }
        };
    }

    while i < len {
        match state {
            MlState::BlockComment => {
                if i + 1 < len && chars[i] == '*' && chars[i + 1] == '/' {
                    fill!(i, i + 2, HL_COMMENT);
                    i += 2;
                    state = MlState::Normal;
                } else {
                    out[i] = HL_COMMENT;
                    i += 1;
                }
            }
            MlState::TemplateStr => {
                out[i] = HL_STRING;
                if chars[i] == '`' { state = MlState::Normal; }
                i += 1;
            }
            MlState::PyTripleSingle | MlState::PyTripleDouble => {
                let q = if state == MlState::PyTripleSingle { '\'' } else { '"' };
                if i + 2 < len && chars[i] == q && chars[i+1] == q && chars[i+2] == q {
                    fill!(i, i + 3, HL_STRING);
                    i += 3;
                    state = MlState::Normal;
                } else {
                    out[i] = HL_STRING;
                    i += 1;
                }
            }
            MlState::CodeFence => {
                // CodeFence is only used by highlight_markdown_line; treat as normal here
                state = MlState::Normal;
            }
            // HtmlComment and HtmlTag are used by highlight_html_line and also by the
            // TypeScript/JSX path for multi-line JSX tags and HTML comments in TSX.
            MlState::HtmlComment => {
                if i + 2 < len && chars[i] == '-' && chars[i+1] == '-' && chars[i+2] == '>' {
                    fill!(i, i + 3, HL_COMMENT);
                    i += 3;
                    state = MlState::Normal;
                } else {
                    out[i] = HL_COMMENT;
                    i += 1;
                }
            }
            MlState::HtmlTag => {
                // Inside a JSX/HTML tag: color attributes and values, exit on > or />
                if chars[i] == '>' {
                    i += 1;
                    state = MlState::Normal;
                } else if chars[i] == '/' && i + 1 < len && chars[i+1] == '>' {
                    fill!(i, i + 2, FG);
                    i += 2;
                    state = MlState::Normal;
                } else if chars[i] == '"' || chars[i] == '\'' {
                    let q = chars[i];
                    out[i] = HL_STRING; i += 1;
                    while i < len {
                        out[i] = HL_STRING;
                        if chars[i] == '\\' && i + 1 < len { out[i+1] = HL_STRING; i += 2; }
                        else if chars[i] == q { i += 1; break; }
                        else { i += 1; }
                    }
                } else if chars[i] == '{' {
                    // JSX expression in attribute — leave as FG, exit tag mode to handle braces
                    i += 1;
                    state = MlState::Normal;
                } else if chars[i].is_alphabetic() || chars[i] == '_' || chars[i] == '-' {
                    let start = i;
                    while i < len && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-' || chars[i] == '.') {
                        i += 1;
                    }
                    // Check if this is an attribute name (followed by = or whitespace or >)
                    fill!(start, i, HL_TYPE);
                } else {
                    i += 1;
                }
            }
            MlState::Normal => {
                let py_comment = lang == Lang::Python && chars[i] == '#';
                let rs_comment = (lang == Lang::Rust || lang == Lang::TypeScript)
                    && i + 1 < len && chars[i] == '/' && chars[i + 1] == '/';
                if py_comment || rs_comment {
                    fill!(i, len, HL_COMMENT);
                    i = len;
                    continue;
                }

                if (lang == Lang::Rust || lang == Lang::TypeScript)
                    && i + 1 < len && chars[i] == '/' && chars[i + 1] == '*'
                {
                    fill!(i, i + 2, HL_COMMENT);
                    i += 2;
                    state = MlState::BlockComment;
                    continue;
                }

                if lang == Lang::Python
                    && i + 2 < len
                    && (chars[i] == '"' || chars[i] == '\'')
                    && chars[i + 1] == chars[i]
                    && chars[i + 2] == chars[i]
                {
                    let q = chars[i];
                    let ml = if q == '\'' { MlState::PyTripleSingle } else { MlState::PyTripleDouble };
                    fill!(i, i + 3, HL_STRING);
                    i += 3;
                    state = ml;
                    while i < len {
                        if i + 2 < len && chars[i] == q && chars[i+1] == q && chars[i+2] == q {
                            fill!(i, i + 3, HL_STRING);
                            i += 3;
                            state = MlState::Normal;
                            break;
                        }
                        out[i] = HL_STRING;
                        i += 1;
                    }
                    continue;
                }

                if chars[i] == '"'
                    || (chars[i] == '\'' && lang != Lang::Rust)
                    || (chars[i] == '\'' && lang == Lang::Rust)
                {
                    let q = chars[i];
                    if lang == Lang::Rust && q == '\'' {
                        let mut j = i + 1;
                        let mut found_close = false;
                        while j < len && j < i + 10 {
                            if chars[j] == '\'' { found_close = true; break; }
                            if chars[j] == '\n'  { break; }
                            j += 1;
                        }
                        if found_close {
                            fill!(i, j + 1, HL_STRING);
                            i = j + 1;
                        } else {
                            out[i] = HL_TYPE;
                            i += 1;
                            while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                                out[i] = HL_TYPE;
                                i += 1;
                            }
                        }
                        continue;
                    }

                    out[i] = HL_STRING;
                    i += 1;
                    while i < len {
                        out[i] = HL_STRING;
                        if chars[i] == '\\' && i + 1 < len {
                            out[i + 1] = HL_STRING;
                            i += 2;
                        } else if chars[i] == q {
                            i += 1;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                    continue;
                }

                if lang == Lang::TypeScript && chars[i] == '`' {
                    out[i] = HL_STRING;
                    i += 1;
                    state = MlState::TemplateStr;
                    continue;
                }

                // JSX/TSX: detect < as tag opener (not less-than operator).
                // Heuristic: < followed by letter/slash is a tag; preceded by a
                // space/operator/start-of-line or ( context makes it unambiguous.
                if lang == Lang::TypeScript && chars[i] == '<' {
                    let next = chars.get(i + 1).copied();
                    if next == Some('/') {
                        // Closing tag </Foo> or </div>
                        out[i] = FG; out[i+1] = FG; i += 2;
                        let _start = i;
                        while i < len && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.' || chars[i] == '-') {
                            out[i] = HL_FUNC; i += 1;
                        }
                        continue;
                    } else if next.map_or(false, |c| c.is_alphabetic()) {
                        // Opening tag — peek at tag name to decide component vs element
                        let tag_start = i + 1;
                        let mut j = tag_start;
                        while j < len && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '.' || chars[j] == '-') {
                            j += 1;
                        }
                        // Only treat as JSX if tag name is followed by space, >, /, or newline
                        // (avoids false positives on generics like Vec<i32>)
                        let after_tag = chars.get(j).copied();
                        let is_jsx_tag = matches!(after_tag, Some(' ') | Some('\t') | Some('>') | Some('/') | Some('\n') | None)
                            || j == len; // tag name goes to end of line (multi-line JSX)
                        if is_jsx_tag {
                            let is_component = chars[tag_start].is_uppercase();
                            let tag_color = if is_component { HL_TYPE } else { HL_FUNC };
                            out[i] = FG; i += 1; // '<'
                            fill!(i, j, tag_color);
                            i = j;
                            state = MlState::HtmlTag;
                            continue;
                        }
                    }
                }

                if chars[i].is_ascii_digit()
                    || (chars[i] == '.' && i + 1 < len && chars[i + 1].is_ascii_digit())
                {
                    let start = i;
                    while i < len && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                        i += 1;
                    }
                    fill!(start, i, HL_NUMBER);
                    continue;
                }

                if chars[i].is_alphabetic() || chars[i] == '_' {
                    let start = i;
                    while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                    if lang == Lang::Rust && i < len && chars[i] == '!' {
                        fill!(start, i + 1, HL_FUNC);
                        i += 1;
                        continue;
                    }
                    let is_call = i < len && chars[i] == '(';
                    let word: String = chars[start..i].iter().collect();
                    let color = classify_word(&word, lang, is_call);
                    fill!(start, i, color);
                    continue;
                }

                if lang == Lang::Python && chars[i] == '@' {
                    let start = i;
                    i += 1;
                    while i < len && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                        i += 1;
                    }
                    fill!(start, i, HL_TYPE);
                    continue;
                }

                if rainbow && matches!(chars[i], '(' | '[' | '{') {
                    out[i] = RAINBOW[(bracket_depth.max(0) as usize) % RAINBOW.len()];
                    bracket_depth += 1;
                    i += 1;
                    continue;
                }
                if rainbow && matches!(chars[i], ')' | ']' | '}') {
                    bracket_depth = bracket_depth.saturating_sub(1);
                    out[i] = RAINBOW[(bracket_depth.max(0) as usize) % RAINBOW.len()];
                    i += 1;
                    continue;
                }

                i += 1;
            }
        }
    }

    (out, state, bracket_depth)
}

// ── Clipboard ─────────────────────────────────────────────────────────────────

fn clipboard_set(text: &str) {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
    #[cfg(not(target_os = "macos"))]
    { with_clipboard(|c| { let _ = c.set_text(text.to_owned()); }); }
}
fn clipboard_get() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        Command::new("pbpaste").output().ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
    }
    #[cfg(not(target_os = "macos"))]
    { with_clipboard(|c| c.get_text().ok()).flatten() }
}

/// The process-wide clipboard handle. It is kept alive because on X11 the
/// copied text is served by the owner: dropping it would empty the clipboard.
#[cfg(not(target_os = "macos"))]
fn with_clipboard<R>(f: impl FnOnce(&mut arboard::Clipboard) -> R) -> Option<R> {
    use std::sync::{Mutex, OnceLock};
    static CLIPBOARD: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();
    let mut guard = CLIPBOARD.get_or_init(|| Mutex::new(None)).lock().ok()?;
    if guard.is_none() { *guard = arboard::Clipboard::new().ok(); }
    guard.as_mut().map(f)
}

fn open_file_dialog() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let out = Command::new("osascript")
            .args(["-e", "POSIX path of (choose file)"])
            .output().ok()?;
        if !out.status.success() { return None; }
        let path = String::from_utf8(out.stdout).ok()?;
        Some(PathBuf::from(path.trim()))
    }
    #[cfg(windows)]
    { os::dialogs::open_file(Some(&dialog_start_dir())) }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = dialog_start_dir();
        linux_dialog(&["--file-selection", "--title=Open File"], &["--getopenfilename", &dir])
            .map(PathBuf::from)
    }
}
fn save_file_dialog() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("osascript")
            .args(["-e", "POSIX path of (choose file name with prompt \"Save as\")"])
            .output().ok()?;
        if !out.status.success() { return None; }
        let path = String::from_utf8(out.stdout).ok()?;
        let path = path.trim();
        (!path.is_empty()).then(|| PathBuf::from(path))
    }
    #[cfg(windows)]
    { os::dialogs::save_file(Some(&dialog_start_dir())) }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = dialog_start_dir();
        linux_dialog(&["--file-selection", "--save", "--title=Save As"], &["--getsavefilename", &dir])
            .map(PathBuf::from)
    }
}
fn prompt_for_name(prompt: &str, default: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    let name = {
        let script = r#"
on run argv
    set answer to display dialog (item 1 of argv) default answer (item 2 of argv) buttons {"Cancel", "OK"} default button "OK"
    return text returned of answer
end run
"#;
        let out = std::process::Command::new("osascript")
            .args(["-e", script, "--", prompt, default])
            .output().ok()?;
        if !out.status.success() { return None; }
        String::from_utf8(out.stdout).ok()?.trim().to_owned()
    };
    #[cfg(windows)]
    let name = os::dialogs::prompt(prompt, default)?.trim().to_owned();
    #[cfg(all(unix, not(target_os = "macos")))]
    let name = linux_dialog(
        &["--entry", "--title=local-text", &format!("--text={prompt}"), &format!("--entry-text={default}")],
        &["--inputbox", prompt, default],
    )?.trim().to_owned();
    let path = std::path::Path::new(&name);
    if name.is_empty()
        || matches!(name.as_str(), "." | "..")
        || path.file_name().and_then(|value| value.to_str()) != Some(name.as_str())
    {
        return None;
    }
    Some(name)
}

/// Linux has no built-in dialog API: use zenity (GNOME) or else kdialog (KDE).
/// Returns what the tool printed; `None` when the user cancelled or neither
/// tool is installed (see `dialog_tool_missing`).
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_dialog(zenity: &[&str], kdialog: &[&str]) -> Option<String> {
    for (bin, args) in [("zenity", zenity), ("kdialog", kdialog)] {
        let Ok(out) = os::command(bin).args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else { continue }; // not installed: try the next one
        if !out.status.success() { return None; } // cancelled
        let text = String::from_utf8(out.stdout).ok()?;
        let text = text.trim_end_matches(['\n', '\r']);
        return (!text.is_empty()).then(|| text.to_owned());
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn dialog_start_dir() -> String {
    std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".to_owned())
}

/// File/name dialogs can't be shown at all (Linux without zenity or kdialog).
fn dialog_tool_missing() -> bool {
    if MACOS || cfg!(windows) { return false; }
    os::find_executable("zenity").is_none() && os::find_executable("kdialog").is_none()
}

fn unique_copy_destination(parent: &std::path::Path, source: &std::path::Path) -> PathBuf {
    let file_name = source.file_name().and_then(|name| name.to_str()).unwrap_or("copy");
    let stem = source.file_stem().and_then(|name| name.to_str()).unwrap_or(file_name);
    let extension = source.extension().and_then(|ext| ext.to_str());
    for index in 1usize.. {
        let suffix = if index == 1 { " copy".to_owned() } else { format!(" copy {index}") };
        let candidate_name = match extension {
            Some(ext) if source.is_file() => format!("{stem}{suffix}.{ext}"),
            _ => format!("{file_name}{suffix}"),
        };
        let candidate = parent.join(candidate_name);
        if !candidate.exists() { return candidate; }
    }
    unreachable!()
}

fn paste_destination(parent: &std::path::Path, source: &std::path::Path) -> PathBuf {
    let candidate = parent.join(source.file_name().unwrap_or_else(|| std::ffi::OsStr::new("copy")));
    if candidate.exists() {
        unique_copy_destination(parent, source)
    } else {
        candidate
    }
}

fn copy_path_recursively(source: &std::path::Path, destination: &std::path::Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(std::fs::read_link(source)?, destination)?;
            return Ok(());
        }
        #[cfg(windows)]
        {
            // Windows symlinks are typed; creating one needs Developer Mode or admin.
            let link = std::fs::read_link(source)?;
            return if std::fs::metadata(source).map_or(false, |m| m.is_dir()) {
                std::os::windows::fs::symlink_dir(link, destination)
            } else {
                std::os::windows::fs::symlink_file(link, destination)
            };
        }
        #[cfg(not(any(unix, windows)))]
        return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "symlink copy is unsupported"));
    }
    if metadata.is_file() {
        std::fs::copy(source, destination)?;
        return Ok(());
    }
    if destination.starts_with(source) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cannot copy a folder inside itself",
        ));
    }
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        copy_path_recursively(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn refresh_explorer_now(s: &mut State) {
    if let Some(explorer) = s.explorer.as_mut() {
        explorer.last_refresh = Instant::now();
        explorer.refresh_local();
    }
}

fn update_paths_after_rename(s: &mut State, old_path: &VPath, new_path: &VPath) {
    let affected_documents: HashMap<VPath, Lang> = s.panes.values()
        .flat_map(|pane| pane.tabs.iter())
        .filter_map(|tab| {
            let path = tab.path.as_ref()?;
            path.strip_prefix(old_path)?;
            Some((path.clone(), tab.lang()))
        })
        .collect();
    for (path, lang) in &affected_documents {
        notify_lsp_tab_closed(s, Some(path.clone()), *lang);
    }

    let remap = |path: &VPath| -> Option<VPath> {
        let suffix = path.strip_prefix(old_path)?;
        Some(new_path.join(suffix))
    };
    for pane in s.panes.values_mut() {
        for tab in &mut pane.tabs {
            if let Some(new_tab_path) = tab.path.as_ref().and_then(&remap) {
                tab.path = Some(new_tab_path);
            }
        }
    }
    let old_diagnostics = std::mem::take(&mut s.diagnostics);
    for (path, diagnostics) in old_diagnostics {
        s.diagnostics.insert(remap(&path).unwrap_or(path), diagnostics);
    }
    for (path, _) in &mut s.cursor_back {
        if let Some(new_cursor_path) = remap(path) { *path = new_cursor_path; }
    }
    for (path, _) in &mut s.cursor_fwd {
        if let Some(new_cursor_path) = remap(path) { *path = new_cursor_path; }
    }
    if let Some(new_source) = s.explorer_clipboard.as_ref().and_then(&remap) {
        s.explorer_clipboard = Some(new_source);
    }
    let renamed_documents: Vec<VPath> = affected_documents.keys()
        .filter_map(&remap)
        .collect();
    for path in renamed_documents {
        notify_lsp_open(s, &path);
    }
}

// ── Find helpers ──────────────────────────────────────────────────────────────

/// Case-fold a run of characters, remembering for every folded char which input
/// char it came from (lowercasing can turn one char into several, e.g. 'İ').
fn fold_chars(chars: impl Iterator<Item = char>, case_sensitive: bool) -> (Vec<char>, Vec<usize>) {
    let mut folded = Vec::new();
    let mut origin = Vec::new();
    for (k, ch) in chars.enumerate() {
        if case_sensitive {
            folded.push(ch);
            origin.push(k);
        } else {
            for lc in ch.to_lowercase() {
                folded.push(lc);
                origin.push(k);
            }
        }
    }
    (folded, origin)
}

/// All matches of `query` in `line_chars` as (start, end) char ranges of the
/// *original* characters. A match must start and end on whole original chars.
fn find_in_chars(line_chars: &[char], query: &[char], case_sensitive: bool) -> Vec<(usize, usize)> {
    let (folded, origin) = fold_chars(line_chars.iter().copied(), case_sensitive);
    let q = query.len();
    let mut out = Vec::new();
    if q == 0 || folded.len() < q { return out; }
    let mut i = 0;
    while i + q <= folded.len() {
        let starts_char = i == 0 || origin[i - 1] != origin[i];
        let ends_char = i + q == folded.len() || origin[i + q] != origin[i + q - 1];
        if starts_char && ends_char && folded[i..i + q] == query[..] {
            out.push((origin[i], origin[i + q - 1] + 1));
            i += q;
        } else {
            i += 1;
        }
    }
    out
}

fn find_matches(text: &Rope, query: &str, case_sensitive: bool, whole_word: bool) -> Vec<(usize, usize)> {
    if query.is_empty() { return vec![]; }
    let (query_chars, _) = fold_chars(query.chars(), case_sensitive);
    if query_chars.is_empty() { return vec![]; }
    let total = text.len_chars();
    let mut out = vec![];
    let mut line_start_char: usize = 0;
    let mut line_chars: Vec<char> = Vec::new();
    for line in text.lines() {
        line_chars.clear();
        line_chars.extend(line.chars().take_while(|&c| c != '\n' && c != '\r'));
        for (lo, hi) in find_in_chars(&line_chars, &query_chars, case_sensitive) {
            let (abs_lo, abs_hi) = (line_start_char + lo, line_start_char + hi);
            let ok = !whole_word || {
                let before = abs_lo == 0 || !State::is_word_char(text.char(abs_lo - 1));
                let after  = abs_hi >= total || !State::is_word_char(text.char(abs_hi));
                before && after
            };
            if ok { out.push((abs_lo, abs_hi)); }
        }
        line_start_char += line.len_chars();
    }
    out
}

/// Matching rules for "select next occurrence" (Cmd+D / Cmd+Shift+L): always
/// case-sensitive, and whole-word when the selection is exactly one word (as when
/// it was just expanded from a bare cursor), so `foo` doesn't also grab `Foo` or
/// `foobar`.
fn selection_match_flags(tab: &Tab) -> (bool, bool) {
    let Some((lo, hi)) = tab.sel() else { return (true, false) };
    (true, word_bounds_at(tab, lo) == (lo, hi))
}

fn word_bounds_at(tab: &Tab, pos: usize) -> (usize, usize) {
    let len = tab.text.len_chars();
    let pos = pos.min(len);
    let in_word  = pos < len && State::is_word_char(tab.text.char(pos));
    let at_end   = pos > 0  && State::is_word_char(tab.text.char(pos - 1));
    if !in_word && !at_end { return (pos, pos); }
    let mut lo = pos;
    let mut hi = pos;
    while lo > 0 && State::is_word_char(tab.text.char(lo - 1)) { lo -= 1; }
    while hi < len && State::is_word_char(tab.text.char(hi))   { hi += 1; }
    (lo, hi)
}

fn find_step(s: &mut State, backwards: bool) {
    let (query, cs, ww) = {
        let f = s.find();
        (f.query.clone(), f.case_sensitive, f.whole_word)
    };
    let matches = find_matches(&s.tab().text, &query, cs, ww);
    if matches.is_empty() { return; }
    let idx = if backwards {
        let p = s.tab().primary().lo();
        matches.iter().rposition(|&(lo, _)| lo < p).unwrap_or(matches.len() - 1)
    } else {
        let p = s.tab().primary().hi();
        matches.iter().position(|&(lo, _)| lo >= p).unwrap_or(0)
    };
    let (lo, hi) = matches[idx];
    s.tab_mut().cursors = vec![Cursor { head: hi, tail: lo }];
    s.ensure_visible();
}

fn replace_current(s: &mut State) {
    if s.tab().primary().has_sel() {
        let repl = s.find().replace.clone();
        for ch in repl.chars() { s.glyphs.load(ch); }
        s.insert_str(&repl);
        find_step(s, false);
    }
}

fn replace_all(s: &mut State) {
    if active_editor_tab(s).is_none() { return; }
    let (query, cs, ww) = {
        let f = s.find();
        (f.query.clone(), f.case_sensitive, f.whole_word)
    };
    let matches = find_matches(&s.tab().text, &query, cs, ww);
    if matches.is_empty() { return; }
    let repl = s.find().replace.clone();
    for ch in repl.chars() { s.glyphs.load(ch); }
    // Goes through push_undo like any edit: undoable, re-highlights, re-syncs the
    // find matches, split views, the language server and collab peers.
    s.push_undo(false);
    let edits: Vec<(usize, usize, String)> = matches.iter().map(|&(lo, hi)| (lo, hi, repl.clone())).collect();
    let n = edits.len();
    let tab = s.tab_mut();
    let mut cursors = std::mem::take(&mut tab.cursors);
    apply_char_edits(&mut tab.text, &mut cursors, edits);
    tab.cursors = cursors;
    tab.clamp_cursors();
    tab.dirty = true;
    s.dedup_cursors();
    s.ensure_visible();
    s.status_msg = Some(format!("Replaced {n} occurrence{}", if n == 1 { "" } else { "s" }));
}

// ── Quick finder helpers ──────────────────────────────────────────────────────

fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() { return Some(0); }
    let qchars: Vec<char> = query.to_lowercase().chars().collect();
    let cchars: Vec<char> = candidate.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score: i32 = 0;
    let mut last_match: Option<usize> = None;
    let mut consecutive_run: i32 = 0;
    let mut max_run: i32 = 0;
    for (ci, &c) in cchars.iter().enumerate() {
        if qi < qchars.len() && c == qchars[qi] {
            if let Some(prev) = last_match {
                if ci == prev + 1 {
                    consecutive_run += 1;
                    max_run = max_run.max(consecutive_run);
                } else {
                    consecutive_run = 0;
                }
            } else {
                consecutive_run = 0;
            }
            if ci == 0 || matches!(cchars[ci - 1], '/' | '_' | '-' | '.') { score += 8; }
            score -= ci as i32 / 4;
            last_match = Some(ci);
            qi += 1;
        }
    }
    if qi < qchars.len() { None } else { Some(score + max_run * 8) }
}

fn fuzzy_score_path(query: &str, path_str: &str) -> Option<i32> {
    // Windows paths use `\`; match on `/` throughout (queries may use either).
    let (query, path_str) = (to_slashes(query), to_slashes(path_str));
    let (query, path_str) = (query.as_ref(), path_str.as_ref());
    let depth = path_str.chars().filter(|&c| c == '/').count() as i32;

    // Absolute or home-relative queries: match against full path string
    let abs_query = if query.starts_with('/') || std::path::Path::new(query).is_absolute() {
        Some(query.to_owned())
    } else if query.starts_with("~/") {
        os::expand_home(query).map(|p| to_slashes(&p.to_string_lossy()).into_owned())
    } else {
        None
    };
    if let Some(ref abs_q) = abs_query {
        return fuzzy_score(abs_q, path_str).map(|sc| sc - depth * 2);
    }

    if query.contains('/') {
        let qsegs: Vec<&str> = query.split('/').filter(|s| !s.is_empty()).collect();
        let psegs: Vec<&str> = path_str.split('/').filter(|s| !s.is_empty()).collect();
        if qsegs.len() > psegs.len() { return None; }
        let mut total = 0i32;
        let mut pi = 0usize;
        for (qi, qseg) in qsegs.iter().enumerate() {
            let remaining = qsegs.len() - qi;
            let mut matched = false;
            while pi + remaining <= psegs.len() {
                if let Some(sc) = fuzzy_score(qseg, psegs[pi]) {
                    total += sc;
                    if qi == qsegs.len() - 1 { total += 15; }
                    pi += 1;
                    matched = true;
                    break;
                }
                pi += 1;
            }
            if !matched { return None; }
        }
        Some(total - depth * 2)
    } else {
        let filename = path_str.rsplit('/').next().unwrap_or(path_str);
        let ql = query.to_lowercase();
        let fl = filename.to_lowercase();
        if fl == ql {
            return Some(100 + fuzzy_score(query, filename).unwrap_or(0) - depth * 2);
        }
        if fl.starts_with(ql.as_str()) {
            return Some(50 + fuzzy_score(query, filename).unwrap_or(0) - depth * 2);
        }
        if let Some(sc) = fuzzy_score(query, filename) {
            Some(sc + 20 - depth * 2)
        } else {
            fuzzy_score(query, path_str).map(|sc| sc - depth * 2)
        }
    }
}

fn qf_prev_char(s: &str, cursor: usize) -> usize {
    if cursor == 0 { return 0; }
    let mut i = cursor - 1;
    while !s.is_char_boundary(i) { i -= 1; }
    i
}

fn qf_next_char(s: &str, cursor: usize) -> usize {
    if cursor >= s.len() { return s.len(); }
    let mut i = cursor + 1;
    while i < s.len() && !s.is_char_boundary(i) { i += 1; }
    i
}

fn clamp_byte_index(s: &str, index: usize) -> usize {
    let mut index = index.min(s.len());
    while !s.is_char_boundary(index) { index -= 1; }
    index
}

fn qf_prev_word(s: &str, cursor: usize) -> usize {
    let b = s.as_bytes();
    let mut i = cursor;
    while i > 0 && b[i - 1] == b' ' { i -= 1; }
    while i > 0 && b[i - 1] != b' ' { i -= 1; }
    i
}

fn qf_next_word(s: &str, cursor: usize) -> usize {
    let b = s.as_bytes();
    let mut i = cursor;
    while i < b.len() && b[i] != b' ' { i += 1; }
    while i < b.len() && b[i] == b' ' { i += 1; }
    i
}

/// Given a monospaced field, compute the byte offset from a pixel x click.
/// text_start_x: pixel where text begins; cw: char width; current_cursor_chars and
/// vis_chars determine the current scroll position.
fn field_click_to_byte(field: &str, mx: i32, text_start_x: i32, cw: i32,
                       current_cursor_chars: usize, vis_chars: usize) -> usize {
    let hscroll = current_cursor_chars.saturating_sub(vis_chars.saturating_sub(1));
    let clicked_char = ((mx - text_start_x) / cw + hscroll as i32).max(0) as usize;
    field.char_indices().nth(clicked_char).map(|(b, _)| b).unwrap_or(field.len())
}

fn input_field_edit(
    field:  &mut String,
    cursor: &mut usize,
    sel:    &mut Option<usize>,
    key:    &Key,
    cmd:    bool,
    alt:    bool,
    ctrl:   bool,
    shift:  bool,
) -> bool {
    // Word-wise: Option on macOS, Ctrl (or Alt) on Linux. Whole line: Cmd on
    // macOS; Linux uses Home/End and Ctrl+Shift+Backspace.
    let word = alt || (!MACOS && ctrl);
    let line = MACOS && cmd;
    // Snapshot selection range before any mutation to avoid borrow conflicts
    let sel_range: Option<(usize, usize)> = sel.and_then(|a| {
        let mn = a.min(*cursor);
        let mx = a.max(*cursor);
        if mn < mx { Some((mn, mx)) } else { None }
    });

    match key {
        Key::Named(NamedKey::ArrowLeft) => {
            if !shift {
                if let Some((mn, _)) = sel_range {
                    *cursor = mn; *sel = None; return true;
                }
                *sel = None;
            } else if sel.is_none() {
                *sel = Some(*cursor);
            }
            *cursor = if word { qf_prev_word(field, *cursor) }
                      else if line { 0 }
                      else { qf_prev_char(field, *cursor) };
            true
        }
        Key::Named(NamedKey::ArrowRight) => {
            if !shift {
                if let Some((_, mx)) = sel_range {
                    *cursor = mx; *sel = None; return true;
                }
                *sel = None;
            } else if sel.is_none() {
                *sel = Some(*cursor);
            }
            *cursor = if word { qf_next_word(field, *cursor) }
                      else if line { field.len() }
                      else { qf_next_char(field, *cursor) };
            true
        }
        Key::Named(NamedKey::Home) => {
            if shift && sel.is_none() { *sel = Some(*cursor); }
            *cursor = 0;
            if !shift { *sel = None; }
            true
        }
        Key::Named(NamedKey::End) => {
            if shift && sel.is_none() { *sel = Some(*cursor); }
            *cursor = field.len();
            if !shift { *sel = None; }
            true
        }
        Key::Named(NamedKey::Delete) => {
            if let Some((mn, mx)) = sel_range {
                field.drain(mn..mx); *cursor = mn; *sel = None;
            } else {
                let next = if word { qf_next_word(field, *cursor) } else { qf_next_char(field, *cursor) };
                if next > *cursor { field.drain(*cursor..next); }
            }
            true
        }
        Key::Named(NamedKey::Backspace) => {
            if let Some((mn, mx)) = sel_range {
                field.drain(mn..mx); *cursor = mn; *sel = None;
            } else if line || (!MACOS && ctrl && shift) {
                field.clear(); *cursor = 0; *sel = None;
            } else if word {
                let nc = qf_prev_word(field, *cursor);
                field.drain(nc..*cursor); *cursor = nc;
            } else if *cursor > 0 {
                let nc = qf_prev_char(field, *cursor);
                field.drain(nc..*cursor); *cursor = nc;
            }
            true
        }
        Key::Named(NamedKey::Space) if !cmd && !ctrl => {
            if let Some((mn, mx)) = sel_range { field.drain(mn..mx); *cursor = mn; *sel = None; }
            field.insert_str(*cursor, " "); *cursor += 1;
            true
        }
        Key::Character(c) => match c.as_str() {
            "a" if cmd => {
                *sel = Some(0); *cursor = field.len(); true
            }
            "c" if cmd => {
                if let Some((mn, mx)) = sel_range { clipboard_write(&field[mn..mx]); }
                true
            }
            "x" if cmd => {
                if let Some((mn, mx)) = sel_range {
                    let text = field[mn..mx].to_owned();
                    clipboard_write(&text);
                    field.drain(mn..mx); *cursor = mn; *sel = None;
                }
                true
            }
            "v" if cmd => {
                let text = clipboard_read();
                if !text.is_empty() {
                    if let Some((mn, mx)) = sel_range {
                        field.drain(mn..mx); *cursor = mn; *sel = None;
                    }
                    field.insert_str(*cursor, &text);
                    *cursor += text.len();
                }
                true
            }
            s if !cmd && !ctrl && (MACOS || !alt) => {
                if let Some((mn, mx)) = sel_range {
                    field.drain(mn..mx); *cursor = mn; *sel = None;
                }
                field.insert_str(*cursor, s); *cursor += s.len();
                true
            }
            _ => false,
        },
        _ => false,
    }
}

const WALK_FILE_CAP: usize = 50_000;

/// Collect project files for Cmd+P, global search and the collab file list.
/// Skips hidden entries, `node_modules` and Cargo `target` dirs; doesn't follow
/// directory symlinks (a link cycle would otherwise multiply the walk at every
/// level); and stops entirely once the cap is reached.
fn walk_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
    if depth > 12 || out.len() >= WALK_FILE_CAP { return; }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if out.len() >= WALK_FILE_CAP { return; }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" { continue; }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if name_str == "target" && path.join("CACHEDIR.TAG").exists() { continue; }
            walk_files(&path, out, depth + 1);
        } else if ft.is_file() || (ft.is_symlink() && path.is_file()) {
            out.push(path);
        }
    }
}

fn open_quick_finder(s: &mut State, proxy: EventLoopProxy<UserEvent>) {
    let remote_root = s.explorer.as_ref().and_then(|e| {
        if let VPath::Remote { host, path } = &e.root {
            Some((host.clone(), path.clone()))
        } else {
            None
        }
    });
    let local_root = if remote_root.is_none() {
        Some(s.explorer.as_ref()
            .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()))
    } else {
        None
    };

    s.quick_finder.walk_token += 1;
    let token = s.quick_finder.walk_token;
    s.quick_finder.entries             = vec![];
    s.quick_finder.filtered            = vec![];
    s.quick_finder.filtered_commands   = vec![];
    s.quick_finder.query               = String::new();
    s.quick_finder.cursor              = 0;
    s.quick_finder.sel_anchor          = None;
    s.quick_finder.selected            = 0;
    s.quick_finder.loading             = true;
    s.quick_finder.remote_directory_mode = false;
    // The finder is modal and sees keys before any sidebar field, so whatever had
    // focus keeps it and gets it back if the finder is dismissed.
    s.quick_finder.open                = true;

    s.quick_finder.collab_mode = false;
    // A collab guest picks from the host's shared files.
    if s.collab.as_ref().map_or(false, |sess| !sess.is_host()) {
        s.quick_finder.collab_mode = true;
        s.quick_finder.entries = s.collab_file_list.iter().map(|rel| VPath::Local(PathBuf::from(rel))).collect();
        s.quick_finder.filtered = (0..s.quick_finder.entries.len()).collect();
        s.quick_finder.loading = false;
        return;
    }
    if let Some((host, root)) = remote_root {
        ssh::ssh_walk_files(host, root, token, proxy);
    } else if let Some(root) = local_root {
        std::thread::spawn(move || {
            let mut local_entries = Vec::new();
            walk_files(&root, &mut local_entries, 0);
            let entries: Vec<VPath> = local_entries.into_iter().map(VPath::Local).collect();
            let _ = proxy.send_event(UserEvent::QuickFinderFiles { token, entries });
        });
    }
}

/// Dismiss the quick finder / palette without acting (Escape, click outside it).
/// A ':' go-to-line preview puts the cursor back where it was.
fn quick_finder_cancel(s: &mut State) {
    let qf = &mut s.quick_finder;
    if qf.query.starts_with(':') {
        if let (Some(saved), Some(pid), Some(tidx)) = (qf.preview_saved_char, qf.preview_pane_id, qf.preview_tab_idx) {
            if let Some(tab) = s.panes.get_mut(&pid).and_then(|p| p.tabs.get_mut(tidx)) {
                tab.cursors = vec![Cursor::new(saved)];
                tab.clamp_cursors();
            }
        }
    }
    let qf = &mut s.quick_finder;
    qf.preview_pane_id    = None;
    qf.preview_tab_idx    = None;
    qf.preview_saved_char = None;
    qf.open = false;
    qf.remote_directory_mode = false;
}

/// Act on the quick finder's selected entry (Enter, or a click on a row).
/// Returns true when that closed the last pane and the app should exit.
fn quick_finder_accept(s: &mut State) -> bool {
    let idx = s.quick_finder.selected;
    if s.quick_finder.query.starts_with('>') {
        let picked = match s.quick_finder.filtered_commands.get(idx) {
            Some(QfCommand::Static(fi)) => Some(Ok(COMMANDS[*fi].action)),
            Some(QfCommand::LspOutput { pane_id, .. }) => Some(Err(*pane_id)),
            None => None,
        };
        match picked {
            Some(Ok(action)) => {
                s.quick_finder.open = false;
                if matches!(action, CommandAction::CloseTab) { return close_active_tab(s); }
                execute_command(s, action);
            }
            Some(Err(pane_id)) => {
                s.quick_finder.open = false;
                show_lsp_output_pane(s, pane_id);
                blur_sidebar(s);
            }
            None => {}
        }
        return false;
    }
    if s.quick_finder.query.starts_with(':') {
        // ':' go-to-line mode: the live preview already moved the cursor.
        if let Some((line, col)) = parse_goto_line_query(&s.quick_finder.query.clone()) {
            s.quick_finder.preview_saved_char = None; // keep position on Enter
            s.quick_finder.open = false;
            s.quick_finder.preview_pane_id    = None;
            s.quick_finder.preview_tab_idx    = None;
            blur_sidebar(s);
            // Re-apply jump in case preview_pane_id was unset
            goto_line_preview(s, line, col.unwrap_or(0));
            s.ensure_visible();
        }
        return false;
    }

    let query = s.quick_finder.query.trim().to_owned();
    let typed_remote = if query.starts_with("ssh://") {
        Some(parse_remote_directory_query(&query))
    } else {
        None
    };
    // After opening a file, jump to the parsed line:col if present.
    fn maybe_jump(s: &mut State, path: VPath) {
        if let Some(line) = s.quick_finder.parsed_line {
            let col = s.quick_finder.parsed_col.unwrap_or(0);
            jump_to_path_line_col(s, path, line, col);
        }
    }
    if s.quick_finder.remote_directory_mode {
        s.quick_finder.open = false;
        s.quick_finder.remote_directory_mode = false;
        if let Some(result) = typed_remote {
            match result {
                Ok((vpath, uri)) => open_remote_directory(s, vpath, uri),
                Err(msg) => s.status_msg = Some(msg),
            }
        } else if let Some(&fi) = s.quick_finder.filtered.get(idx) {
            let path = s.quick_finder.entries[fi].clone();
            if let Some(uri) = remote_uri(&path) {
                open_remote_directory(s, path, uri);
            }
        } else {
            s.status_msg = Some("Enter a remote URI like ssh://host:~/devel".to_owned());
        }
    } else if query.starts_with("lt-collab://") {
        // "Join Collab Session…" pre-fills this prefix; joining reads the
        // invite from the query and closes the finder.
        execute_command(s, CommandAction::JoinCollab);
    } else if let Some(result) = typed_remote {
        s.quick_finder.open = false;
        match result {
            Ok((vpath, uri)) => open_remote_directory(s, vpath, uri),
            Err(msg) => s.status_msg = Some(msg),
        }
    } else if let Some(direct) = s.quick_finder.direct_path.clone().filter(|_| idx == 0 && s.quick_finder.filtered.is_empty()) {
        // Direct absolute/~ path with no fuzzy results
        s.quick_finder.open = false;
        push_cursor_history(s);
        open_or_reuse_tab(s, direct.clone());
        maybe_jump(s, direct);
    } else if let Some(&fi) = s.quick_finder.filtered.get(idx) {
        let path = s.quick_finder.entries[fi].clone();
        s.quick_finder.open = false;
        // If the selected entry is a remote path shown from "Open Remote Directory",
        // open it as a workspace root rather than a file tab.
        let is_remote_dir_mode = query.starts_with("ssh://")
            && matches!(&path, VPath::Remote { .. });
        if is_remote_dir_mode {
            if let Some(uri) = remote_uri(&path) {
                open_remote_directory(s, path, uri);
            }
        } else if s.quick_finder.collab_mode {
            open_collab_file(s, path.as_path().to_string_lossy().into_owned());
        } else {
            push_cursor_history(s);
            open_or_reuse_tab(s, path.clone());
            maybe_jump(s, path);
        }
    }
    // Whatever was opened gets the keyboard, not a sidebar field focused before.
    if !s.quick_finder.open { blur_sidebar(s); }
    false
}

// Split "path/file.rs:42:5" into ("path/file.rs", Some(41), Some(4)) (0-indexed).
// Only strips trailing colon-delimited segments that are purely numeric.
fn split_path_line_col(q: &str) -> (&str, Option<usize>, Option<usize>) {
    let bytes = q.as_bytes();
    // Find last ':' where everything after is digits
    let find_numeric_suffix = |s: &str| -> Option<usize> {
        let idx = s.rfind(':')?;
        if s[idx + 1..].bytes().all(|b| b.is_ascii_digit()) && !s[idx + 1..].is_empty() {
            Some(idx)
        } else {
            None
        }
    };
    let _ = bytes; // suppress unused warning
    let col_sep = find_numeric_suffix(q);
    let (path_and_line, col) = if let Some(ci) = col_sep {
        let col_num: usize = q[ci + 1..].parse().unwrap_or(1);
        (&q[..ci], Some(col_num.saturating_sub(1)))
    } else {
        (q, None)
    };
    let line_sep = find_numeric_suffix(path_and_line);
    let (path, line) = if let Some(li) = line_sep {
        let line_num: usize = path_and_line[li + 1..].parse().unwrap_or(1);
        (&path_and_line[..li], Some(line_num.saturating_sub(1)))
    } else {
        (path_and_line, None)
    };
    // If we only found one numeric suffix treat it as line (not col)
    if col.is_some() && line.is_none() {
        // col_sep actually held the line number; path_and_line == path_full minus that
        return (path_and_line, col, None);
    }
    (path, line, col)
}

fn parse_goto_line_query(q: &str) -> Option<(usize, Option<usize>)> {
    let rest = q.strip_prefix(':')?;
    let mut parts = rest.splitn(2, ':');
    let line_str = parts.next()?;
    if line_str.is_empty() { return None; }
    let line: usize = line_str.parse().ok()?;
    let col: Option<usize> = parts.next().and_then(|s| s.parse().ok());
    Some((line.saturating_sub(1), col.map(|c| c.saturating_sub(1))))
}

// Move cursor in active editor tab for ':' go-to-line live preview (no file open).
fn goto_line_preview(s: &mut State, line: usize, col: usize) {
    let Some(pane_id) = s.quick_finder.preview_pane_id else { return };
    let Some(tab_idx) = s.quick_finder.preview_tab_idx else { return };
    let Some(pane) = s.panes.get_mut(&pane_id) else { return };
    let Some(tab) = pane.tabs.get_mut(tab_idx) else { return };
    let target_line = line.min(State::last_line(&tab.text));
    let target_col  = col.min(State::line_len(&tab.text, target_line));
    let pos = tab.text.line_to_char(target_line) + target_col;
    tab.cursors = vec![Cursor::new(pos)];
}

fn refilter_quick_finder(s: &mut State) {
    let q = s.quick_finder.query.clone();
    if q.starts_with('>') {
        let cq = q[1..].trim().to_owned();
        // Build static command entries
        let static_cmds: Vec<QfCommand> = if cq.is_empty() {
            (0..COMMANDS.len()).map(QfCommand::Static).collect()
        } else {
            let mut scored: Vec<(usize, i32)> = COMMANDS.iter().enumerate()
                .filter_map(|(i, cmd)| fuzzy_score(&cq, cmd.name).map(|sc| (i, sc)))
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            scored.into_iter().map(|(i, _)| QfCommand::Static(i)).collect()
        };
        // Build dynamic LSP output entries, one per running server
        let lsp_cmds: Vec<QfCommand> = s.lsp.servers.values()
            .filter_map(|srv| {
                let title = s.term_panes.get(&srv.output_pane_id)?.title.clone();
                let name = format!("Open LSP Output: {}", title);
                if cq.is_empty() || fuzzy_score(&cq, &name).is_some() {
                    Some(QfCommand::LspOutput { pane_id: srv.output_pane_id, title })
                } else { None }
            })
            .collect();
        s.quick_finder.filtered_commands = static_cmds.into_iter().chain(lsp_cmds).collect();
        s.quick_finder.filtered = vec![];
        s.quick_finder.parsed_line = None;
        s.quick_finder.parsed_col  = None;
        s.quick_finder.direct_path = None;
    } else if q.starts_with(':') {
        // Go-to-line mode: live preview
        s.quick_finder.filtered = vec![];
        s.quick_finder.filtered_commands = vec![];
        s.quick_finder.parsed_line = None;
        s.quick_finder.parsed_col  = None;
        s.quick_finder.direct_path = None;
        if let Some((line, col)) = parse_goto_line_query(&q) {
            goto_line_preview(s, line, col.unwrap_or(0));
        }
    } else if q.is_empty() {
        s.quick_finder.filtered = (0..s.quick_finder.entries.len()).collect();
        s.quick_finder.filtered_commands = vec![];
        s.quick_finder.parsed_line = None;
        s.quick_finder.parsed_col  = None;
        s.quick_finder.direct_path = None;
    } else {
        let remote_directory_mode = s.quick_finder.remote_directory_mode;
        // When in a remote workspace (not the host-selection mode), get the SSH host so we
        // can expand ~/path using the remote user's home instead of the local $HOME.
        let remote_host: Option<vpath::SshHost> = if !remote_directory_mode {
            s.explorer.as_ref().and_then(|ex| ex.root.ssh_host().cloned())
        } else {
            None
        };
        // Strip :line:col suffix before matching
        let (path_part, parsed_line, parsed_col) = split_path_line_col(&q);
        s.quick_finder.parsed_line = parsed_line;
        s.quick_finder.parsed_col  = parsed_col;
        // For remote workspaces, expand ~/path using the remote user's home directory so
        // that queries like "~/.bashrc" match remote entries like "/home/user/.bashrc".
        let remote_expanded = remote_host.as_ref()
            .and_then(|host| path_part.strip_prefix("~/").map(|rest| {
                let home = host.user.as_deref()
                    .map(|u| format!("/home/{u}"))
                    .unwrap_or_else(|| String::from("~"));
                format!("{home}/{rest}")
            }));
        let effective_path_part: &str = remote_expanded.as_deref().unwrap_or(path_part);
        // Absolute/home path: build a direct_path entry.
        // In remote workspaces we create a VPath::Remote without checking local FS.
        // In local workspaces we only show it if the path actually exists.
        let direct_path: Option<VPath> = if let Some(ref host) = remote_host {
            let expanded = if let Some(rest) = path_part.strip_prefix("~/") {
                let home = host.user.as_deref()
                    .map(|u| format!("/home/{u}"))
                    .unwrap_or_else(|| String::from("~"));
                Some(format!("{home}/{rest}"))
            } else if path_part.starts_with('/') {
                Some(path_part.to_owned())
            } else {
                None
            };
            expanded.map(|p| VPath::Remote {
                host: host.clone(),
                path: std::path::PathBuf::from(p),
            })
        } else {
            let expanded = if let Some(home_rel) = os::expand_home(path_part) {
                Some(home_rel)
            } else if path_part.starts_with('/') || std::path::Path::new(path_part).is_absolute() {
                Some(std::path::PathBuf::from(path_part))
            } else {
                None
            };
            expanded.and_then(|pb| {
                if pb.exists() { Some(VPath::Local(pb)) } else { None }
            })
        };
        s.quick_finder.direct_path = direct_path;
        let mut scored: Vec<(usize, i32)> = s.quick_finder.entries.iter().enumerate()
            .filter_map(|(i, p)| {
                let path_str = if remote_directory_mode {
                    remote_uri(p).unwrap_or_else(|| p.as_path().to_string_lossy().into_owned())
                } else {
                    p.as_path().to_string_lossy().into_owned()
                };
                fuzzy_score_path(effective_path_part, &path_str).map(|sc| (i, sc))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        s.quick_finder.filtered = scored.into_iter().map(|(i, _)| i).collect();
        s.quick_finder.filtered_commands = vec![];
    }
    s.quick_finder.selected = 0;
}

/// Walk the local explorer root on a background thread for the tree filter;
/// delivers `UserEvent::TreeSearchFiles`.
fn spawn_tree_search_walk(ex: &mut FileExplorer, proxy: &EventLoopProxy<UserEvent>) {
    let Some(root) = ex.root.as_local_path().map(|p| p.to_path_buf()) else { return };
    if ex.tree_search_walking {
        ex.tree_search_rewalk = true;
        return;
    }
    ex.tree_search_walking = true;
    ex.tree_search_rewalk = false;
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let mut entries = Vec::new();
        walk_files(&root, &mut entries, 0);
        let _ = proxy.send_event(UserEvent::TreeSearchFiles { root, entries });
    });
}

fn refilter_tree_search(ex: &mut FileExplorer) {
    if ex.tree_search.is_empty() {
        ex.tree_search_results.clear();
        ex.tree_search_sel = 0;
        return;
    }
    let q = ex.tree_search.to_lowercase();
    let mut scored: Vec<(usize, i32)> = ex.tree_search_entries.iter().enumerate()
        .filter_map(|(i, p)| {
            let path_str = p.to_string_lossy();
            let sc = if ex.tree_search_fuzzy {
                fuzzy_score_path(&q, &path_str)?
            } else {
                let name = path_basename(&path_str);
                if name.to_lowercase().contains(&q as &str) { 0i32 } else { return None; }
            };
            Some((i, sc))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.truncate(200);
    ex.tree_search_results = scored.into_iter().map(|(i, _)| ex.tree_search_entries[i].clone()).collect();
    ex.tree_search_sel = 0;
}

// ── Collab ────────────────────────────────────────────────────────────────────
//
// Shared documents are named by workspace-relative path on both sides. The host's
// copy is its normal tab for `<root>/<path>`; a guest's copy is a tab with
// `collab_path` set (no file path — the guest never writes it to its own disk).

/// Host: workspace-relative ('/'-separated) path for a local file under the
/// explorer root.
fn host_rel_path(s: &State, path: &VPath) -> Option<String> {
    let root = s.explorer.as_ref()?.root.as_local_path()?;
    let rel = path.as_local_path()?.strip_prefix(root).ok()?;
    let rel = rel.to_str()?.replace(std::path::MAIN_SEPARATOR, "/");
    collab::is_safe_rel_path(&rel).then_some(rel)
}

/// The shared-document path a tab corresponds to in the current session.
fn tab_collab_rel(s: &State, tab: &Tab) -> Option<String> {
    if tab.kind != TabKind::Editor { return None; }
    if let Some(rel) = &tab.collab_path { return Some(rel.clone()); }
    if !s.collab.as_ref()?.is_host() { return None; }
    host_rel_path(s, tab.path.as_ref()?)
}

/// The tab holding shared document `rel` (host: `<root>/rel`; guest: collab_path).
fn find_collab_tab(s: &State, rel: &str) -> Option<(usize, usize)> {
    let sess = s.collab.as_ref()?;
    if sess.is_host() {
        let root = s.explorer.as_ref()?.root.as_local_path()?.to_path_buf();
        return find_tab_for_path(s, &VPath::Local(root.join(rel)));
    }
    let mut ids: Vec<usize> = s.panes.keys().copied().collect();
    ids.sort_by_key(|id| (*id != s.active_pane, *id));
    ids.into_iter().find_map(|pid| {
        s.panes[&pid].tabs.iter()
            .position(|t| t.kind == TabKind::Editor && t.collab_path.as_deref() == Some(rel))
            .map(|ti| (pid, ti))
    })
}

fn find_tab_by_buf(s: &State, buf_id: usize) -> Option<(usize, usize)> {
    let mut ids: Vec<usize> = s.panes.keys().copied().collect();
    ids.sort_by_key(|id| (*id != s.active_pane, *id));
    ids.into_iter().find_map(|pid| {
        let pane = &s.panes[&pid];
        if pid == s.active_pane && pane.tabs.get(pane.active).map_or(false, |t| t.buf_id == buf_id) {
            return Some((pid, pane.active));
        }
        pane.tabs.iter().position(|t| t.buf_id == buf_id).map(|ti| (pid, ti))
    })
}

/// Guest: may we edit this shared tab? (Viewers and hidden files without
/// WRITE_HIDDEN are read-only; the host would reject the edits anyway.)
fn collab_tab_read_only(s: &State, tab: &Tab) -> bool {
    let (Some(rel), Some(sess)) = (tab.collab_path.as_deref(), s.collab.as_ref()) else { return false };
    match sess.my_role {
        Some(role) => !collab::guest_can_write(rel, role, &sess.role_perms),
        None => false,
    }
}

/// The TextOperation turning `before` into `after` (one contiguous change, found
/// with the same chunk-level prefix/suffix scan the highlighter uses).
fn diff_to_operation(before: &Rope, after: &Rope) -> Option<collab::TextOperation> {
    let (lb, la) = (before.len_bytes(), after.len_bytes());
    let pb = common_prefix_bytes(before, after);
    if pb == lb && pb == la { return None; }
    // Whole chars only: byte_to_char floors to the char containing `pb`.
    let prefix_chars = before.byte_to_char(pb.min(lb));
    let prefix_byte = before.char_to_byte(prefix_chars);
    let sb = common_suffix_bytes(before, after, lb.min(la) - prefix_byte);
    let start_b = lb - sb;
    let mut c = before.byte_to_char(start_b);
    if before.char_to_byte(c) != start_b { c += 1; }
    let suffix_chars = before.len_chars() - c;
    let deleted = before.len_chars() - prefix_chars - suffix_chars;
    let inserted: String = after.slice(prefix_chars..after.len_chars() - suffix_chars).to_string();
    Some(collab::TextOperation::from_change(prefix_chars, deleted, &inserted, suffix_chars))
}

fn transform_remote_cursors(sess: &mut collab::CollabSession, rel: &str, op: &collab::TextOperation) {
    for (path, cursors) in sess.remote_cursors.values_mut() {
        if path != rel { continue; }
        for c in cursors.iter_mut() {
            c.head = op.transform_index(c.head);
            c.tail = op.transform_index(c.tail);
        }
    }
}

/// Apply a peer's operation to every tab showing the buffer, moving cursors with it.
/// Returns false (nothing changed) if the operation doesn't fit the buffer.
fn apply_remote_op_to_buffer(s: &mut State, pane_id: usize, tab_idx: usize, op: &collab::TextOperation) -> bool {
    let Some(tab) = s.panes.get(&pane_id).and_then(|p| p.tabs.get(tab_idx)) else { return false };
    let buf_id = tab.buf_id;
    let host = s.collab.as_ref().map_or(false, |c| c.is_host());
    let mut text = tab.text.clone();
    if op.apply(&mut text).is_err() { return false; }
    for p in s.panes.values_mut() {
        for t in p.tabs.iter_mut().filter(|t| t.buf_id == buf_id && t.kind == TabKind::Editor) {
            t.note_edit();
            t.text = text.clone();
            for c in &mut t.cursors {
                c.head = op.transform_index(c.head);
                c.tail = op.transform_index(c.tail);
            }
            t.clamp_cursors();
            if host { t.dirty = true; } // the host owns the file; guests never save
        }
    }
    notify_lsp_change_for(s, pane_id, tab_idx);
    s.needs_redraw = true;
    true
}

/// Send the buffer's local edit (since `collab_before` was captured) to peers.
/// Called after every event that may have edited text.
fn notify_collab_change(s: &mut State) {
    let Some((buf_id, before)) = s.collab_before.take() else { return };
    if s.collab.is_none() { return; }
    let Some((pid, ti)) = find_tab_by_buf(s, buf_id) else { return };
    let tab = &s.panes[&pid].tabs[ti];
    let Some(rel) = tab_collab_rel(s, tab) else { return };
    let Some(op) = diff_to_operation(&before, &tab.text) else { return };
    if tab.collab_path.is_some() {
        // Guest copy: edits go to the host, which owns (and saves) the file.
        for p in s.panes.values_mut() {
            for t in p.tabs.iter_mut().filter(|t| t.buf_id == buf_id) { t.dirty = false; }
        }
    }
    let Some(sess) = s.collab.as_mut() else { return };
    transform_remote_cursors(sess, &rel, &op);
    if sess.is_host() {
        sess.host_local_edit(&rel, op);
    } else {
        sess.client_local_edit(&rel, op);
    }
}

/// Send a debounced CursorUpdate for the active shared document.
fn send_collab_cursor_if_needed(s: &mut State) {
    let Some(sess) = s.collab.as_ref() else { return };
    if !sess.cursor_debounce_elapsed() { return; }
    let Some(ti) = active_editor_tab(s) else { return };
    let tab = &s.panes[&s.active_pane].tabs[ti];
    let Some(rel) = tab_collab_rel(s, tab) else { return };
    let cursors: Vec<collab::RemoteCursorPos> = tab.cursors.iter()
        .map(|c| collab::RemoteCursorPos { head: c.head, tail: c.tail })
        .collect();
    let Some(sess) = s.collab.as_mut() else { return };
    let msg = collab::CollabMsg::CursorUpdate { site_id: sess.site_id, cursors, path: rel.clone() };
    if sess.is_host() {
        let targets = sess.guests_with_doc(&rel);
        if targets.is_empty() { return; }
        sess.broadcast_where(None, &msg, |sid| targets.contains(&sid));
    } else if sess.client_docs.contains_key(&rel) {
        sess.send_to_site(0, &msg);
    }
    sess.last_cursor_sent = Instant::now();
}

/// Turn a guest's shared tab into an ordinary unsaved buffer (session ended or
/// access revoked) so its contents aren't silently lost.
fn detach_collab_tabs(s: &mut State, only: Option<&str>) {
    for p in s.panes.values_mut() {
        for t in p.tabs.iter_mut() {
            if t.collab_path.is_some() && only.map_or(true, |rel| t.collab_path.as_deref() == Some(rel)) {
                t.collab_path = None;
                t.dirty = true;
            }
        }
    }
    if let Some(sess) = s.collab.as_mut() {
        match only {
            Some(rel) => { sess.client_docs.remove(rel); }
            None => sess.client_docs.clear(),
        }
    }
}

/// A tab is about to close: tell peers we no longer have that document open.
fn collab_tab_closing(s: &mut State, pane_id: usize, tab_idx: usize) {
    if s.collab.is_none() || has_buffer_sibling(s, pane_id, tab_idx) { return; }
    let Some(tab) = s.panes.get(&pane_id).and_then(|p| p.tabs.get(tab_idx)) else { return };
    let Some(rel) = tab_collab_rel(s, tab) else { return };
    let Some(sess) = s.collab.as_mut() else { return };
    if sess.is_host() {
        if sess.server_docs.remove(&rel).is_none() { return; }
        let holders: Vec<u64> = sess.open_docs.iter().filter(|(_, d)| d.contains(&rel)).map(|(&sid, _)| sid).collect();
        for sid in holders {
            sess.send_to_site(sid, &collab::CollabMsg::FileClosedByHost { path: rel.clone() });
            if let Some(d) = sess.open_docs.get_mut(&sid) { d.remove(&rel); }
        }
        sess.remote_cursors.retain(|_, (p, _)| p != &rel);
    } else {
        sess.client_docs.remove(&rel);
        sess.send_to_site(0, &collab::CollabMsg::FileClosed { path: rel });
    }
}

/// Guest: open (or switch to) shared document `rel`, requesting it from the host.
fn open_collab_file(s: &mut State, rel: String) {
    if let Some((pid, ti)) = find_collab_tab(s, &rel) {
        s.active_pane = pid;
        if let Some(p) = s.panes.get_mut(&pid) { p.active = ti; }
        return;
    }
    let ap = ensure_editor_pane(s);
    let mut tab = Tab::untitled(s.next_buf_id);
    s.next_buf_id += 1;
    tab.collab_path = Some(rel.clone());
    let pane = s.panes.get_mut(&ap).unwrap();
    if pane.tab().is_empty_untitled() && !pane.tab().dirty {
        let active = pane.active;
        pane.tabs[active] = tab;
    } else {
        pane.tabs.push(tab);
        pane.active = pane.tabs.len() - 1;
    }
    if let Some(sess) = s.collab.as_ref() {
        sess.send_to_site(0, &collab::CollabMsg::FileRequest { path: rel });
    }
}

/// Guest: install a document snapshot from the host (first load or resync).
fn guest_load_doc(s: &mut State, doc: collab::DocSnapshot) {
    let Some(sess) = s.collab.as_mut() else { return };
    let resync = sess.client_docs.contains_key(&doc.path);
    sess.client_docs.insert(doc.path.clone(), collab::ClientDoc { rev: doc.rev, ..Default::default() });
    let text = Rope::from_str(&doc.content);
    match find_collab_tab(s, &doc.path) {
        Some((pid, ti)) => {
            let buf_id = s.panes[&pid].tabs[ti].buf_id;
            for p in s.panes.values_mut() {
                for t in p.tabs.iter_mut().filter(|t| t.buf_id == buf_id) {
                    t.replace_text(text.clone());
                    t.dirty = false;
                }
            }
            if resync { s.status_msg = Some(format!("{}: resynchronised with the host", doc.path)); }
        }
        None => {
            let ap = ensure_editor_pane(s);
            let mut tab = Tab::untitled(s.next_buf_id);
            s.next_buf_id += 1;
            tab.collab_path = Some(doc.path.clone());
            tab.replace_text(text);
            let pane = s.panes.get_mut(&ap).unwrap();
            pane.tabs.push(tab);
            pane.active = pane.tabs.len() - 1;
        }
    }
    s.needs_redraw = true;
}

/// Host: open `path` in a tab without switching to it (a guest asked for it).
fn open_background_tab(s: &mut State, path: VPath) -> Option<(usize, usize)> {
    if let Some(found) = find_tab_for_path(s, &path) { return Some(found); }
    let pid = if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Editor) {
        s.active_pane
    } else {
        let mut ids: Vec<usize> = s.panes.iter().filter(|(_, p)| p.kind == PaneKind::Editor).map(|(&id, _)| id).collect();
        ids.sort_unstable();
        *ids.first()?
    };
    let mut tab = Tab::untitled(s.next_buf_id);
    tab.load_file(path.clone()).ok()?;
    s.next_buf_id += 1;
    let pane = s.panes.get_mut(&pid)?;
    pane.tabs.push(tab);
    let ti = pane.tabs.len() - 1;
    notify_lsp_open(s, &path);
    Some((pid, ti))
}

/// Host: send `site_id` the authoritative copy of `rel` (first open or resync),
/// or a denial if it may not see it.
fn host_send_doc(s: &mut State, site_id: u64, rel: &str) {
    let allowed = s.collab.as_ref().map_or(false, |sess| sess.guest_may_view(site_id, rel));
    let root = s.explorer.as_ref().and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()));
    let deny = |s: &mut State| {
        if let Some(sess) = s.collab.as_mut() {
            if let Some(d) = sess.open_docs.get_mut(&site_id) { d.remove(rel); }
            sess.send_to_site(site_id, &collab::CollabMsg::FileDenied { path: rel.to_owned() });
        }
    };
    let (true, Some(root)) = (allowed, root) else { return deny(s) };
    // Sandbox: the file must resolve inside the workspace (no `..`, absolute
    // paths or symlinks pointing out).
    if collab::resolve_under_root(&root, rel).is_none() { return deny(s); }
    let Some((pid, ti)) = find_collab_tab(s, rel).or_else(|| open_background_tab(s, VPath::Local(root.join(rel)))) else {
        return deny(s);
    };
    let content: String = s.panes[&pid].tabs[ti].text.chunks().collect();
    let Some(sess) = s.collab.as_mut() else { return };
    let rev = sess.server_docs.entry(rel.to_owned()).or_default().rev;
    sess.open_docs.entry(site_id).or_default().insert(rel.to_owned());
    sess.send_to_site(site_id, &collab::CollabMsg::FileResponse {
        doc: collab::DocSnapshot { path: rel.to_owned(), content, rev },
    });
}

/// Host: a guest's edit. The sender is identified by its connection
/// (`from_site_id`), never by anything inside the message. The edit is checked
/// for permission *before* it touches the host's buffer, rebased onto every
/// revision the guest hadn't seen, applied, then acknowledged and forwarded.
fn host_handle_guest_op(s: &mut State, from_site_id: u64, path: String, base_rev: u64, op: collab::TextOperation) {
    let Some(sess) = s.collab.as_ref() else { return };
    let has_open = sess.open_docs.get(&from_site_id).map_or(false, |d| d.contains(&path));
    if !has_open || !sess.guest_may_write(from_site_id, &path) {
        // Not allowed: reset the guest's copy to the authoritative one.
        return host_send_doc(s, from_site_id, &path);
    }
    let rebased = sess.server_docs.get(&path).and_then(|doc| doc.rebase(base_rev, op));
    let Some(op) = rebased else { return host_send_doc(s, from_site_id, &path) };
    let Some((pid, ti)) = find_collab_tab(s, &path) else { return host_send_doc(s, from_site_id, &path) };
    if !apply_remote_op_to_buffer(s, pid, ti, &op) {
        return host_send_doc(s, from_site_id, &path);
    }
    if let Some(sess) = s.collab.as_mut() {
        transform_remote_cursors(sess, &path, &op);
        sess.host_commit_guest_op(from_site_id, &path, op);
    }
}

/// Guest: an edit from the host.
fn guest_handle_remote_op(s: &mut State, path: String, rev: u64, op: collab::TextOperation) {
    let Some(sess) = s.collab.as_mut() else { return };
    if !sess.client_docs.contains_key(&path) { return; } // not open here
    let Some(op) = sess.client_remote_op(&path, rev, op) else {
        sess.send_to_site(0, &collab::CollabMsg::FileRequest { path });
        return;
    };
    transform_remote_cursors(sess, &path, &op);
    let applied = find_collab_tab(s, &path).map_or(false, |(pid, ti)| apply_remote_op_to_buffer(s, pid, ti, &op));
    if !applied {
        // Our copy diverged (shouldn't happen): fetch a fresh one.
        if let Some(sess) = s.collab.as_ref() {
            sess.send_to_site(0, &collab::CollabMsg::FileRequest { path });
        }
    }
}

/// Host: build the list of files `site_id` may open, off the UI thread.
fn spawn_collab_file_list(s: &State, site_id: u64) {
    let (Some(sess), Some(root)) = (
        s.collab.as_ref(),
        s.explorer.as_ref().and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf())),
    ) else { return };
    let Some(role) = sess.peer_role(site_id) else { return };
    let (perms_snap, include, exclude) = (sess.role_perms.clone(), sess.include_globs.clone(), sess.exclude_globs.clone());
    let proxy = s.proxy.clone();
    std::thread::spawn(move || {
        let mut raw = Vec::new();
        walk_files(&root, &mut raw, 0);
        let paths: Vec<String> = raw.into_iter()
            .filter_map(|p| p.strip_prefix(&root).ok().and_then(|r| r.to_str()).map(|r| r.replace(std::path::MAIN_SEPARATOR, "/")))
            .filter(|rel| collab::is_safe_rel_path(rel)
                && collab::guest_can_view(rel, role, &perms_snap, &include, &exclude)
                // Only files the guest could actually open (not symlinks/aliases).
                && collab::resolve_under_root(&root, rel).is_some())
            .collect();
        let _ = proxy.send_event(UserEvent::CollabFileListReady { site_id, paths });
    });
}

/// Host: roles, permissions or globs changed — revoke documents guests may no
/// longer see and send them fresh file lists.
fn host_refresh_guest_access(s: &mut State, only: Option<u64>) {
    let Some(sess) = s.collab.as_mut() else { return };
    if !sess.is_host() { return; }
    let sites: Vec<u64> = sess.peers.iter().map(|p| p.site_id).filter(|sid| only.map_or(true, |o| o == *sid)).collect();
    for &sid in &sites {
        let revoked: Vec<String> = sess.open_docs.get(&sid)
            .map(|docs| docs.iter().filter(|rel| !sess.guest_may_view(sid, rel)).cloned().collect())
            .unwrap_or_default();
        for rel in revoked {
            if let Some(d) = sess.open_docs.get_mut(&sid) { d.remove(&rel); }
            sess.send_to_site(sid, &collab::CollabMsg::FileDenied { path: rel });
        }
    }
    for sid in sites { spawn_collab_file_list(s, sid); }
}

/// Host: copy role permissions and share globs from settings into the running
/// session (that copy is what's enforced), broadcast the new permissions, and
/// re-check what each guest may see.
fn sync_collab_settings(s: &mut State) {
    let Some(sess) = s.collab.as_mut() else { return };
    if !sess.is_host() { return; }
    let perms_changed = {
        let (a, b) = (&sess.role_perms, &s.settings.collab_role_perms);
        (a.viewer, a.contributor, a.system_access, a.moderator, a.default_role)
            != (b.viewer, b.contributor, b.system_access, b.moderator, b.default_role)
    };
    let globs_changed = sess.include_globs != s.settings.collab_include_globs
        || sess.exclude_globs != s.settings.collab_exclude_globs;
    if !perms_changed && !globs_changed { return; }
    sess.role_perms = s.settings.collab_role_perms.clone();
    sess.include_globs = s.settings.collab_include_globs.clone();
    sess.exclude_globs = s.settings.collab_exclude_globs.clone();
    if perms_changed {
        let msg = collab::CollabMsg::RoleConfig { perms: sess.role_perms.clone() };
        sess.broadcast_except_msg(None, &msg);
    }
    host_refresh_guest_access(s, None);
}

fn handle_collab_msg(s: &mut State, from_site_id: u64, msg: collab::CollabMsg) {
    use collab::CollabMsg;
    // Pending local edits must go out (based on the current revision) before
    // anything from a peer is applied.
    notify_collab_change(s);
    let Some(is_host) = s.collab.as_ref().map(|c| c.is_host()) else { return };
    match msg {
        CollabMsg::Op { path, base_rev, op } if is_host => {
            host_handle_guest_op(s, from_site_id, path, base_rev, op);
        }
        CollabMsg::RemoteOp { path, rev, op, .. } if !is_host => {
            guest_handle_remote_op(s, path, rev, op);
        }
        CollabMsg::Ack { path, rev } if !is_host => {
            if let Some(sess) = s.collab.as_mut() { sess.client_ack(&path, rev); }
        }
        CollabMsg::CursorUpdate { site_id, cursors, path } => {
            let Some(sess) = s.collab.as_mut() else { return };
            let cursors: Vec<_> = cursors.into_iter().take(256).collect();
            if is_host {
                if !sess.open_docs.get(&from_site_id).map_or(false, |d| d.contains(&path)) { return; }
                sess.remote_cursors.insert(from_site_id, (path.clone(), cursors.clone()));
                let targets = sess.guests_with_doc(&path);
                let relay = CollabMsg::CursorUpdate { site_id: from_site_id, cursors, path };
                sess.broadcast_where(Some(from_site_id), &relay, |sid| targets.contains(&sid));
            } else {
                sess.remote_cursors.insert(site_id, (path, cursors));
            }
        }
        CollabMsg::PeerJoined { peer } if !is_host => {
            let name = peer.name.clone();
            if let Some(session) = s.collab.as_mut() {
                if !session.peers.iter().any(|p| p.site_id == peer.site_id) {
                    session.peers.push(peer);
                }
            }
            s.status_msg = Some(format!("{name} joined the session"));
        }
        CollabMsg::PeerLeft { site_id } if !is_host => {
            if let Some(session) = s.collab.as_mut() {
                session.peers.retain(|p| p.site_id != site_id);
                session.remote_cursors.remove(&site_id);
            }
        }
        CollabMsg::PeerRoleChanged { site_id, role } if !is_host => {
            if let Some(session) = s.collab.as_mut() {
                if let Some(peer) = session.peers.iter_mut().find(|p| p.site_id == site_id) {
                    peer.role = role;
                }
                if site_id == session.site_id {
                    session.my_role = Some(role);
                }
            }
        }
        CollabMsg::RoleConfig { perms } if !is_host => {
            if let Some(session) = s.collab.as_mut() {
                session.role_perms = perms;
            }
        }
        CollabMsg::FileList { paths } if !is_host => {
            s.collab_file_list = paths;
        }
        CollabMsg::FileRequest { path } if is_host => {
            host_send_doc(s, from_site_id, &path);
        }
        CollabMsg::FileClosed { path } if is_host => {
            if let Some(sess) = s.collab.as_mut() {
                if let Some(d) = sess.open_docs.get_mut(&from_site_id) { d.remove(&path); }
                if let Some((p, _)) = sess.remote_cursors.get(&from_site_id) {
                    if p == &path { sess.remote_cursors.remove(&from_site_id); }
                }
            }
        }
        CollabMsg::FileResponse { doc } if !is_host => {
            guest_load_doc(s, doc);
        }
        CollabMsg::FileDenied { path } if !is_host => {
            detach_collab_tabs(s, Some(&path));
            s.status_msg = Some(format!("Access denied: {path}"));
        }
        CollabMsg::FileClosedByHost { path } if !is_host => {
            detach_collab_tabs(s, Some(&path));
            s.status_msg = Some(format!("The host closed {path}; your copy is now a local, unsaved buffer"));
        }
        CollabMsg::RoleChangeRequest { target_site_id, new_role } if is_host => {
            // Only a peer whose role has MANAGE_ROLES may ask, and never for a role
            // above Contributor.
            let requester_perms = s.collab.as_ref().map_or(0, |sess| sess.peer_perms(from_site_id));
            let can_manage = requester_perms & collab::perms::MANAGE_ROLES != 0;
            let target_too_high = matches!(new_role, collab::PeerRole::SystemAccess | collab::PeerRole::Moderator);
            if !can_manage || target_too_high { return; }
            if let Some(session) = s.collab.as_mut() {
                if let Some(peer) = session.peers.iter_mut().find(|p| p.site_id == target_site_id) {
                    peer.role = new_role;
                }
                let msg = CollabMsg::PeerRoleChanged { site_id: target_site_id, role: new_role };
                session.broadcast_except_msg(None, &msg);
            }
            host_refresh_guest_access(s, Some(target_site_id));
        }
        _ => {} // not valid in this direction: ignore
    }
}

/// Paste into the active editor: with N cursors and N clipboard lines, one line
/// goes to each cursor; otherwise the whole text is inserted at every cursor.
fn paste_text(s: &mut State, text: &str) {
    let n_cursors = s.tab().cursors.len();
    let lines: Vec<&str> = text.split('\n').collect();
    if n_cursors > 1 && lines.len() == n_cursors {
        let owned: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        s.insert_str_per_cursor(&owned);
    } else {
        s.insert_str(text);
    }
}

/// Open the active editor buffer in a new pane beside the current one. The new
/// tab shares the buffer (`buf_id`) so edits in either view stay in sync.
fn split_active_editor(s: &mut State, zone: DropZone) {
    let Some(ti) = active_editor_tab(s) else {
        s.status_msg = Some("Split: focus an editor tab first".to_owned());
        return;
    };
    let active = s.active_pane;
    let src = &s.panes[&active].tabs[ti];
    let new_id = s.next_pane_id; s.next_pane_id += 1;
    let mut p  = Pane::new(new_id, src.buf_id);
    {
        let t = &mut p.tabs[0];
        t.text        = src.text.clone();
        t.path        = src.path.clone();
        t.dirty       = src.dirty;
        t.collab_path = src.collab_path.clone();
        t.cursors     = src.cursors.clone();
        t.scroll      = src.scroll;
        t.lsp_synced_gen = Some(t.edit_generation); // same text the server already has
    }
    s.panes.insert(new_id, p);
    let old = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
    s.pane_tree = insert_pane(old, active, new_id, zone);
    s.active_pane = new_id;
}

/// Index of the active tab if the active pane is an editor pane showing a normal
/// editor tab (not Settings / git diff). Commands that read or edit the buffer use
/// this so they never index a terminal pane's (empty) tab list.
fn active_editor_tab(s: &State) -> Option<usize> {
    let pane = s.panes.get(&s.active_pane)?;
    if pane.kind != PaneKind::Editor { return None; }
    let tab = pane.tabs.get(pane.active)?;
    (tab.kind == TabKind::Editor).then_some(pane.active)
}

/// The active tab is a text buffer this user may edit (not settings, a git
/// diff, or a shared file their collab role can only view).
fn active_tab_editable(s: &State) -> bool {
    active_editor_tab(s).is_some() && !collab_tab_read_only(s, s.tab())
}

fn settings_tab_active(s: &State) -> bool {
    s.panes.get(&s.active_pane)
        .filter(|p| p.kind == PaneKind::Editor)
        .and_then(|p| p.tabs.get(p.active))
        .map_or(false, |t| t.kind == TabKind::Settings)
}

/// Take keyboard focus away from every sidebar input and list so keys go to
/// the active pane again.
fn blur_sidebar(s: &mut State) {
    if let Some(ex) = s.explorer.as_mut() { ex.tree_search_focused = false; }
    s.git_panel.commit_focused = false;
    s.global_find.focus = GlobalFindFocus::Results;
    s.sidebar_list_focused = false;
}

/// Store the settings text field being edited (if any) and stop editing it.
fn commit_settings_field(s: &mut State) {
    let Some(field_id) = s.settings_edit_field.take() else { return };
    let text = std::mem::take(&mut s.settings_edit_text);
    s.settings_edit_cursor = 0;
    let globs = |text: &str| -> Vec<String> {
        text.split(',').map(|g| g.trim().to_owned()).filter(|g| !g.is_empty()).collect()
    };
    match field_id {
        SettingsFieldId::FormatOnSave          => s.settings.format_on_save = text,
        SettingsFieldId::OrganizeImportsOnSave => s.settings.organize_imports_on_save = text,
        SettingsFieldId::FormatCommand         => s.settings.format_command = text,
        SettingsFieldId::CollabPort            => {
            if let Ok(p) = text.trim().parse::<u16>() { s.settings.collab_port = p; }
        }
        SettingsFieldId::CollabIncludeGlobs    => s.settings.collab_include_globs = globs(&text),
        SettingsFieldId::CollabExcludeGlobs    => s.settings.collab_exclude_globs = globs(&text),
    }
    s.settings.save();
    sync_collab_settings(s);
}

const MACOS: bool = cfg!(target_os = "macos");

/// The app-shortcut modifier ("cmd" throughout the key handling): Command on
/// macOS, Control on Linux and Windows.
fn primary_mod(m: ModifiersState) -> bool {
    if MACOS { m.super_key() } else { m.control_key() }
}

/// Shortcut labels are written macOS-style ("Cmd+S", "Opt+Shift+F"); show the
/// Linux/Windows names there.
fn key_label(label: &str) -> std::borrow::Cow<'_, str> {
    if MACOS || !(label.contains("Cmd") || label.contains("Opt+")) {
        return std::borrow::Cow::Borrowed(label);
    }
    std::borrow::Cow::Owned(label.replace("Cmd+", "Ctrl+").replace("Opt+", "Alt+"))
}

/// A sidebar text field (tree search, commit message, a search field) on
/// screen has keyboard focus.
fn sidebar_text_field_focused(s: &State) -> bool {
    s.explorer.as_ref().map_or(false, |ex| s.left_panel_visible && match s.left_view {
        LeftView::FileTree     => ex.tree_search_focused,
        LeftView::Git          => s.git_panel.commit_focused,
        LeftView::GlobalSearch => s.global_find.focus != GlobalFindFocus::Results,
        _ => false,
    })
}

/// The active terminal is what the user is typing into (no overlay or sidebar
/// field has the keyboard).
fn terminal_has_keyboard(s: &State) -> bool {
    s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Terminal)
        && !s.quick_finder.open && s.context_menu.is_none() && s.lsp_panel.is_none()
        && !sidebar_text_field_focused(s)
}

/// Windows has no AltGr modifier: it arrives as Ctrl+Alt. A key that, with both
/// held, produces a different character than the bare key (AltGr+Q → "@" on a
/// German layout) is typing, not a Ctrl+Alt shortcut.
#[cfg(windows)]
fn is_altgr_text(event: &winit::event::KeyEvent, mods: ModifiersState) -> bool {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
    if !(mods.control_key() && mods.alt_key()) { return false; }
    let Key::Character(c) = &event.logical_key else { return false };
    let printable = event.text.as_deref().map_or(false, |t| !t.is_empty() && !t.chars().any(char::is_control));
    printable && !matches!(event.key_without_modifiers(), Key::Character(ref b) if b.eq_ignore_ascii_case(c))
}

/// Other platforms report AltGr as its own modifier, never as Ctrl+Alt.
#[cfg(not(windows))]
fn is_altgr_text(_event: &winit::event::KeyEvent, _mods: ModifiersState) -> bool { false }

/// A key event that only reports a modifier going down.
fn is_modifier_key(key: &Key) -> bool {
    matches!(key, Key::Named(k) if matches!(k,
        NamedKey::Super | NamedKey::Hyper | NamedKey::Meta
        | NamedKey::Control | NamedKey::Alt | NamedKey::AltGraph | NamedKey::Shift
        | NamedKey::CapsLock | NamedKey::Fn | NamedKey::FnLock))
}

/// App-level shortcuts (save, close, new/open/switch tab, split, find/find next) that must
/// keep working while a sidebar or settings field has keyboard focus. Mirrors
/// the matchers in the pane key handlers. Note that with Cmd held winit reports
/// the shifted character, e.g. "}" for Cmd+Shift+].
fn is_app_shortcut(key: &Key, cmd: bool, ctrl: bool, shift: bool) -> bool {
    match key {
        Key::Named(NamedKey::Tab) => ctrl,
        Key::Character(c) => {
            let c = c.as_str();
            ((cmd || ctrl) && c == "s")
                || (cmd && matches!(c, "w" | "t" | "n" | "N" | "o" | "f" | "h" | "g" | "G"
                    | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
                || (cmd && !shift && c == "\\")
                || (cmd && shift && matches!(c, "_" | "]" | "}" | "[" | "{"))
        }
        _ => false,
    }
}

/// Whether a sidebar input/list should let this key through to the pane
/// handlers. Shortcuts that switch to another tab or pane also move keyboard
/// focus there; save and close leave it in the sidebar.
fn sidebar_lets_through(s: &mut State, key: &Key, cmd: bool, ctrl: bool, shift: bool) -> bool {
    if !is_app_shortcut(key, cmd, ctrl, shift) { return false; }
    let keeps_focus = matches!(key, Key::Character(c) if matches!(c.as_str(), "s" | "w"));
    if !keeps_focus { blur_sidebar(s); }
    true
}

/// Rows of global-search results that fit below the search fields (same
/// geometry as the click handler and renderer).
fn global_find_visible_rows(s: &State) -> usize {
    let lh = s.glyphs.lh.max(1);
    let row_h = lh + 2;
    let results_start_y = 4 + 4 * row_h + 2 + row_h + 4 + lh;
    ((s.h as i32 - s.status_h() - results_start_y) / lh).max(1) as usize
}

fn global_find_reveal_selected(s: &mut State) {
    let vis = global_find_visible_rows(s);
    let gf = &mut s.global_find;
    if gf.selected < gf.scroll {
        gf.scroll = gf.selected;
    } else if gf.selected >= gf.scroll + vis {
        gf.scroll = gf.selected + 1 - vis;
    }
}

/// Change the UI font size (Cmd+= / Cmd+- / Cmd+0 and the palette commands).
fn set_font_size(s: &mut State, size: f32) {
    let size = size.clamp(8.0, 36.0);
    if size == s.font_size { return; }
    let ratio = size / s.font_size;
    s.font_size = size;
    s.explorer_w = ((s.explorer_w as f32 * ratio).round() as i32).clamp(80, 600);
    s.settings.font_size = size;
    s.settings.save();
    s.rebuild_glyphs();
    // Cell size changed: terminals must be told their new grid size.
    resize_terminal_panes(s);
}

/// Close the active tab of whatever pane has focus (Cmd+W, palette "Close Tab").
/// LSP output panes are hidden rather than closed. Returns true when the last
/// pane went away and the app should exit.
fn close_active_tab(s: &mut State) -> bool {
    let pane_id = s.active_pane;
    let Some((kind, tab_idx)) = s.panes.get(&pane_id).map(|p| (p.kind.clone(), p.active)) else { return false };
    match kind {
        PaneKind::Terminal if s.lsp.servers.contains_key(&pane_id) => {
            if hide_lsp_output_pane(s, pane_id) {
                // That was the only visible pane: show an editor instead of nothing.
                ensure_editor_pane(s);
            }
            false
        }
        PaneKind::Terminal => {
            let exit = close_terminal_tab(s, pane_id, tab_idx);
            if !exit { check_lsp_binaries_if_settings_open(s); }
            exit
        }
        PaneKind::MarkdownPreview => {
            s.md_panes.remove(&pane_id);
            s.panes.remove(&pane_id);
            remove_pane_from_layout(s, pane_id)
        }
        PaneKind::Editor => request_close_editor_tab(s, pane_id, tab_idx) == Some(true),
    }
}

/// Write one tab to disk (asking for a file name for an untitled tab where a
/// dialog is available). Failures are reported in the status bar.
fn save_tab(s: &mut State, pane_id: usize, tab_idx: usize) -> bool {
    let proxy = s.proxy.clone();
    let needs_name = s.panes.get(&pane_id).and_then(|p| p.tabs.get(tab_idx))
        .map_or(false, |t| t.kind == TabKind::Editor && t.path.is_none() && t.collab_path.is_none());
    if needs_name {
        let Some(chosen) = save_file_dialog() else {
            s.status_msg = Some(if dialog_tool_missing() {
                "Untitled file not saved: install zenity or kdialog for file dialogs".to_owned()
            } else {
                "Untitled file not saved (no file name chosen)".to_owned()
            });
            return false;
        };
        if let Some(t) = s.panes.get_mut(&pane_id).and_then(|p| p.tabs.get_mut(tab_idx)) {
            t.path = Some(VPath::Local(chosen));
        }
    }
    let Some(tab) = s.panes.get_mut(&pane_id).and_then(|p| p.tabs.get_mut(tab_idx)) else { return false };
    let result = tab.save_with_proxy(Some(&proxy));
    let path = tab.path.clone();
    match result {
        Ok(()) => {
            propagate_to_siblings(s, pane_id, tab_idx);
            if let Some(path) = path {
                if needs_name { notify_lsp_open(s, &path); }
                // Saving the project's LSP config from inside the editor means the
                // user has seen and accepted its contents.
                let (root, _) = current_project_target(s);
                if let Some(root) = root {
                    if path.as_local_path().is_some()
                        && root.join(proj_config::CONFIG_REL_PATH).same_file(&path)
                    {
                        trust_project_lsp_config(s);
                    }
                }
            }
            true
        }
        Err(e) => {
            s.status_msg = Some(e);
            false
        }
    }
}

fn execute_command(s: &mut State, action: CommandAction) {
    match action {
        CommandAction::Save           => {
            let pid = s.active_pane;
            let Some(ti) = active_editor_tab(s) else { return };
            let (lang, path) = { let t = &s.panes[&pid].tabs[ti]; (t.lang(), t.path.clone()) };
            // Format-on-save only runs for local files.
            let path_str = path.as_ref()
                .and_then(|p| p.as_local_path())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let glob_hit = |globs: &str| !globs.is_empty() && lang != Lang::None
                && globs.split(',').map(|g| g.trim()).any(|g| glob_match(g, &path_str));
            let want_fmt = glob_hit(&s.settings.format_on_save);
            let want_org = glob_hit(&s.settings.organize_imports_on_save);
            let is_json = matches!(lang, Lang::Json | Lang::Jsonc);
            // JSON is formatted in-process, so do it before writing: the file on disk
            // is the formatted one.
            if want_fmt && is_json { format_json_document(s); }
            if !save_tab(s, pid, ti) { return; }
            // LSP formatting is asynchronous: request it now and write the file again
            // when its edits are applied (see `PendingKind::*::save_after`).
            let lsp_fmt = want_fmt && !is_json;
            if let (Some(p), true) = (path.as_ref(), want_org || lsp_fmt) {
                notify_lsp_change_for(s, pid, ti); // server must see exactly what we saved
                let gen = s.panes[&pid].tabs[ti].edit_generation;
                if let Some(srv) = s.lsp.server_for_lang_host_mut(lang, p.ssh_host()) {
                    if srv.initialized {
                        if want_org { lsp::request_organize_imports(srv, p, gen, lsp_fmt, true); }
                        else        { lsp::request_formatting(srv, p, gen, true); }
                    }
                }
            }

            let fmt_cmd = s.settings.format_command.clone();
            if !fmt_cmd.is_empty() {
                // External format command only supported for local files.
                if let Some(ref p) = path {
                    if let Some(local) = p.as_local_path() {
                        let path_clone = p.clone();
                        let generation = s.panes[&pid].tabs[ti].edit_generation;
                        let local_str = local.to_string_lossy().into_owned();
                        let proxy = s.proxy.clone();
                        let cmd_str = build_format_command(&fmt_cmd, &local_str);
                        std::thread::spawn(move || {
                            let _ = os::shell_command(&cmd_str)
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .status();
                            let _ = proxy.send_event(UserEvent::FormatterDone { path: path_clone, generation });
                        });
                    }
                }
            }
        }
        CommandAction::CloseTab       => { /* needs the event loop to exit: see quick_finder_accept */ }
        CommandAction::GoToSettings   => open_settings_tab(s),
        CommandAction::OpenTerminal   => open_terminal_pane(s),
        CommandAction::SplitRight     => split_active_editor(s, DropZone::Right),
        CommandAction::SplitDown      => split_active_editor(s, DropZone::Bottom),
        CommandAction::SplitLeft      => split_active_editor(s, DropZone::Left),
        CommandAction::SplitUp        => split_active_editor(s, DropZone::Top),
        CommandAction::ToggleFind     => {
            if active_editor_tab(s).is_none() { return; }
            blur_sidebar(s);
            let open = !s.find().open;
            let f = s.find_mut();
            f.open = open;
            f.input_focused = open;
            f.cursor_query = clamp_byte_index(&f.query, f.cursor_query);
            f.sel_anchor_q = None;
        }
        CommandAction::ToggleReplace  => {
            if active_editor_tab(s).is_none() { return; }
            blur_sidebar(s);
            let open = s.find().open;
            s.find_mut().open = true;
            s.find_mut().input_focused = true;
            s.find_mut().replace_open = !open || !s.find().replace_open;
        }
        CommandAction::ToggleExplorer => {
            if s.explorer.is_none() {
                let root = std::env::current_dir().unwrap_or_default();
                let root_vpath = VPath::Local(root);
                s.explorer = Some(FileExplorer::new(root_vpath.clone()));
                start_lsp_for_project(s, &root_vpath);
            } else {
                s.explorer = None;
            }
        }
        CommandAction::ToggleSidebar => {
            if s.explorer.is_some() {
                s.left_panel_visible = !s.left_panel_visible;
                if !s.left_panel_visible { blur_sidebar(s); }
            }
        }
        CommandAction::IncreaseFontSize => set_font_size(s, s.font_size + 2.0),
        CommandAction::DecreaseFontSize => set_font_size(s, s.font_size - 2.0),
        CommandAction::GotoDefinition | CommandAction::FindReferences => {
            let Some(ti) = active_editor_tab(s) else { return };
            let pid = s.active_pane;
            notify_lsp_change_for(s, pid, ti); // server must see the current text
            let tab = &s.panes[&pid].tabs[ti];
            let lang = tab.lang();
            let Some(p) = tab.path.clone() else { return };
            let (line, col) = char_to_lsp_position(&tab.text, tab.primary().head);
            if lang == Lang::None { return; }
            if let Some(srv) = s.lsp.server_for_lang_host_mut(lang, p.ssh_host()) {
                if srv.initialized {
                    if matches!(action, CommandAction::GotoDefinition) {
                        lsp::request_definition(srv, &p, line, col);
                    } else {
                        lsp::request_references(srv, &p, line, col);
                    }
                }
            }
        }
        CommandAction::Copy => {
            if active_editor_tab(s).is_none() { return; }
            let order = s.cursor_order_ltr();
            let texts: Vec<String> = order.iter()
                .filter(|&&i| s.tab().cursors[i].has_sel())
                .map(|&i| { let lo = s.tab().cursors[i].lo(); let hi = s.tab().cursors[i].hi(); s.tab().text.slice(lo..hi).chars().collect() })
                .collect();
            if !texts.is_empty() { clipboard_set(&texts.join("\n")); }
        }
        CommandAction::Cut => {
            if !active_tab_editable(s) { return; }
            let order = s.cursor_order_ltr();
            let texts: Vec<String> = order.iter()
                .filter(|&&i| s.tab().cursors[i].has_sel())
                .map(|&i| { let lo = s.tab().cursors[i].lo(); let hi = s.tab().cursors[i].hi(); s.tab().text.slice(lo..hi).chars().collect() })
                .collect();
            if !texts.is_empty() {
                clipboard_set(&texts.join("\n"));
                s.push_undo(false);
                let mut delta: isize = 0;
                for &i in &order {
                    if s.tab().cursors[i].has_sel() {
                        let lo = apply_delta(s.tab().cursors[i].lo(), delta, s.tab().text.len_chars());
                        let hi = apply_delta(s.tab().cursors[i].hi(), delta, s.tab().text.len_chars());
                        s.tab_mut().text.remove(lo..hi);
                        s.tab_mut().cursors[i] = Cursor::new(lo);
                        delta -= (hi - lo) as isize;
                        s.tab_mut().dirty = true;
                    }
                }
                s.dedup_cursors();
                s.ensure_visible();
            }
        }
        CommandAction::Paste => {
            if !active_tab_editable(s) { return; }
            if let Some(text) = clipboard_get() {
                for ch in text.chars() { s.glyphs.load(ch); }
                paste_text(s, &text);
            }
        }
        CommandAction::CursorBack    => cursor_go_back(s),
        CommandAction::CursorForward => cursor_go_fwd(s),
        CommandAction::FormatDocument => {
            let Some(ti) = active_editor_tab(s) else { return };
            let pid = s.active_pane;
            let lang = s.panes[&pid].tabs[ti].lang();
            match lang {
                Lang::Json | Lang::Jsonc => format_json_document(s),
                Lang::None => {}
                _ => {
                    notify_lsp_change_for(s, pid, ti);
                    let tab = &s.panes[&pid].tabs[ti];
                    let (path, gen) = (tab.path.clone(), tab.edit_generation);
                    if let Some(ref p) = path {
                        if let Some(srv) = s.lsp.server_for_lang_host_mut(lang, p.ssh_host()) {
                            if srv.initialized { lsp::request_formatting(srv, p, gen, false); }
                        }
                    }
                }
            }
        }
        CommandAction::OrganizeImports => {
            let Some(ti) = active_editor_tab(s) else { return };
            let pid = s.active_pane;
            notify_lsp_change_for(s, pid, ti);
            let tab = &s.panes[&pid].tabs[ti];
            let (lang, path, gen) = (tab.lang(), tab.path.clone(), tab.edit_generation);
            if lang != Lang::None {
                if let Some(ref p) = path {
                    if let Some(srv) = s.lsp.server_for_lang_host_mut(lang, p.ssh_host()) {
                        if srv.initialized { lsp::request_organize_imports(srv, p, gen, false, false); }
                    }
                }
            }
        }
        CommandAction::OpenMarkdownPreview => open_markdown_preview(s),
        CommandAction::OpenRemoteDirectory => {
            let query = s.quick_finder.query.trim().to_owned();
            if query.starts_with("ssh://") {
                match parse_remote_directory_query(&query) {
                    Ok((vpath, uri)) => open_remote_directory(s, vpath, uri),
                    Err(msg) => s.status_msg = Some(msg),
                }
                s.quick_finder.open = false;
            } else {
                s.quick_finder.query.clear();
                s.quick_finder.cursor = 0;
                s.quick_finder.sel_anchor = None;
                s.quick_finder.walk_token += 1;
                s.quick_finder.open = true;
                s.quick_finder.remote_directory_mode = true;
                s.quick_finder.entries = s.settings.recent_remote_hosts.iter()
                    .map(|u| VPath::parse(u))
                    .filter(|p| matches!(p, VPath::Remote { .. }))
                    .collect();
                s.quick_finder.filtered = (0..s.quick_finder.entries.len()).collect();
                s.quick_finder.filtered_commands.clear();
                s.quick_finder.selected = 0;
                s.quick_finder.loading = false;
            }
        }
        CommandAction::StartCollab => {
            let port = s.settings.collab_port;
            let ap = s.active_pane;
            let doc_path = s.panes.get(&ap)
                .and_then(|p| p.tabs.get(p.active))
                .and_then(|t| t.path.as_ref())
                .map(|p| p.to_string())
                .unwrap_or_default();
            match collab::start_host(
                port, doc_path, s.proxy.clone(),
                s.settings.collab_role_perms.clone(),
                s.settings.collab_include_globs.clone(),
                s.settings.collab_exclude_globs.clone(),
            ) {
                Ok(session) => {
                    let invite = session.invite_str().to_owned();
                    s.collab = Some(session);
                    s.status_msg = Some(format!("Collab: {invite}"));
                }
                Err(e) => {
                    s.status_msg = Some(format!("Collab error: {e}"));
                }
            }
        }
        CommandAction::JoinCollab => {
            let query = s.quick_finder.query.trim().to_owned();
            if query.starts_with("lt-collab://") {
                let peer_name = whoami_or_default();
                match collab::connect_guest(&query, peer_name, s.proxy.clone()) {
                    Ok(()) => {
                        s.status_msg = Some("Connecting to collab session…".to_owned());
                    }
                    Err(e) => {
                        s.status_msg = Some(format!("Collab error: {e}"));
                    }
                }
                s.quick_finder.open = false;
            } else {
                // Pre-fill quick-finder so user can paste or type the invite string
                s.quick_finder.query = "lt-collab://".to_owned();
                s.quick_finder.cursor = s.quick_finder.query.len();
                s.quick_finder.walk_token += 1;
                s.quick_finder.open = true;
                s.quick_finder.remote_directory_mode = false;
                s.quick_finder.entries = vec![];
                s.quick_finder.filtered = vec![];
                s.quick_finder.selected = 0;
                s.quick_finder.loading = false;
            }
        }
        CommandAction::ManageLspServers => {
            open_lsp_manager(s);
        }
        CommandAction::EditProjectLspConfig => {
            open_project_lsp_config(s);
        }
        CommandAction::TrustProjectLspConfig => trust_project_lsp_config(s),
    }
}

// ── LSP manager panel ─────────────────────────────────────────────────────────

fn open_lsp_manager(s: &mut State) {
    s.quick_finder.open = false;
    let entries = lsp_registry::list_live();
    s.lsp_panel = Some(LspManagerPanel { entries, selected: 0 });
}

/// The project root + host this window would run a server for ("move here" target,
/// and the location of the editable `.vscode/local-text.json`).
fn current_project_target(s: &State) -> (Option<VPath>, Option<vpath::SshHost>) {
    let root = if let Some(ex) = s.explorer.as_ref() {
        Some(ex.root.clone())
    } else {
        let pid = s.active_pane;
        s.panes.get(&pid)
            .filter(|p| p.kind == PaneKind::Editor)
            .and_then(|p| p.tabs.get(p.active))
            .and_then(|t| t.path.clone())
            .and_then(|p| lsp_root_for_path(s, &p))
    };
    let host = root.as_ref().and_then(|r| r.ssh_host().cloned());
    (root, host)
}

fn lsp_panel_selected(s: &State) -> Option<lsp_registry::LspRegistryEntry> {
    s.lsp_panel.as_ref().and_then(|p| p.entries.get(p.selected).cloned())
}

fn lsp_panel_go_to_window(s: &mut State) {
    let Some(entry) = lsp_panel_selected(s) else { s.lsp_panel = None; return; };
    s.lsp_panel = None;
    if entry.pid == std::process::id() {
        s.status_msg = Some("That server is in this window.".to_owned());
        return;
    }
    let pid = entry.pid;
    let label = entry.root.clone().unwrap_or_else(|| entry.lang.clone());
    std::thread::spawn(move || { let _ = lsp_registry::request_raise(pid); });
    s.status_msg = Some(format!("Raising window for {label}…"));
}

fn lsp_panel_stop(s: &mut State) {
    let Some(entry) = lsp_panel_selected(s) else { s.lsp_panel = None; return; };
    let bin = lsp_binary(Lang::from_registry_str(&entry.lang).unwrap_or(Lang::None))
        .unwrap_or("server").to_owned();
    if entry.pid == std::process::id() {
        // Our own server: stop synchronously and refresh the list in place.
        if let Some(mut srv) = s.lsp.servers.remove(&entry.op_id) {
            let _ = srv.process.kill();
            let _ = srv.process.wait();
        }
        lsp_registry::unregister(entry.op_id);
        s.status_msg = Some(format!("Stopped {bin}."));
        if let Some(p) = s.lsp_panel.as_mut() {
            p.entries = lsp_registry::list_live();
            if p.selected >= p.entries.len() { p.selected = p.entries.len().saturating_sub(1); }
        }
    } else {
        let pid = entry.pid;
        let op_id = entry.op_id;
        std::thread::spawn(move || { let _ = lsp_registry::request_stop(pid, op_id); });
        s.status_msg = Some(format!("Stopping {bin} (pid {pid})…"));
        s.lsp_panel = None;
    }
}

fn lsp_panel_move_here(s: &mut State) {
    let Some(entry) = lsp_panel_selected(s) else { s.lsp_panel = None; return; };
    s.lsp_panel = None;
    if entry.pid == std::process::id() {
        s.status_msg = Some("That server is already in this window.".to_owned());
        return;
    }
    let Some(lang) = Lang::from_registry_str(&entry.lang) else { return; };
    let (root, host) = current_project_target(s);
    if root.is_none() {
        s.status_msg = Some("No project open in this window to move the server to.".to_owned());
        return;
    }
    let bin = lsp_binary(lang).unwrap_or("server").to_owned();
    let proxy = s.proxy.clone();
    let pid = entry.pid;
    let op_id = entry.op_id;
    s.status_msg = Some(format!("Moving {bin} here…"));
    std::thread::spawn(move || {
        // Evict the other instance, then ask our main loop to start ours in the
        // freed slot (the start must run on the UI thread, hence the event).
        let _ = lsp_registry::request_stop(pid, op_id);
        let _ = proxy.send_event(UserEvent::LspSlotFreed { lang, ssh_host: host, root });
    });
}

/// Open (creating a template if absent) `<root>/.vscode/local-text.json` for the
/// current local project so the user can edit per-project LSP env/command/args.
fn open_project_lsp_config(s: &mut State) {
    let (root, _) = current_project_target(s);
    let Some(VPath::Local(root)) = root else {
        s.status_msg = Some("Open a local project to edit its LSP settings.".to_owned());
        return;
    };
    let cfg_path = root.join(proj_config::CONFIG_REL_PATH);
    if !cfg_path.exists() {
        if let Some(parent) = cfg_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let template = "{\n  \"rust-analyzer\": {\n    \"env\": {},\n    \"command\": null,\n    \"args\": null\n  }\n}\n";
        let _ = std::fs::write(&cfg_path, template);
    }
    push_cursor_history(s);
    open_or_reuse_tab(s, VPath::Local(cfg_path));
}

// ── Global find/replace ───────────────────────────────────────────────────────

/// Apply a signed `delta` to a character index, saturating at 0 and clamping to `len`.
/// Multi-cursor edit batches accumulate `delta` across cursors; without this a negative
/// running sum would wrap through `as usize` into a huge index and panic the rope op.
#[inline]
fn apply_delta(pos: usize, delta: isize, len: usize) -> usize {
    (pos as isize + delta).clamp(0, len as isize) as usize
}

/// Build the shell command line for the external formatter. `format_command` is the user's
/// own setting, but the file path is interpolated into the platform shell (`sh -c`, or
/// `cmd /c` on Windows — see `os::shell_command`), so it MUST be quoted: otherwise a
/// maliciously-named file (e.g. `$(...).rs` from a cloned repo) would inject commands.
/// `{file}` is substituted if present, otherwise the quoted path is appended.
fn build_format_command(fmt_cmd: &str, file: &str) -> String {
    let quoted = os::shell_quote_path(file);
    if fmt_cmd.contains("{file}") {
        fmt_cmd.replace("{file}", &quoted)
    } else {
        format!("{fmt_cmd} {quoted}")
    }
}

fn glob_match(pattern: &str, path_str: &str) -> bool {
    if pattern.is_empty() { return true; }
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = path_str.chars().collect();
    glob_match_impl(&pat, &txt)
}

fn glob_match_impl(pat: &[char], txt: &[char]) -> bool {
    if pat.is_empty() { return txt.is_empty(); }
    if pat.len() >= 2 && pat[0] == '*' && pat[1] == '*' {
        let rest = &pat[2..];
        let rest = if rest.first() == Some(&'/') { &rest[1..] } else { rest };
        for i in 0..=txt.len() {
            if glob_match_impl(rest, &txt[i..]) { return true; }
            if i < txt.len() && txt[i] == '/' && rest.is_empty() { return true; }
        }
        return false;
    }
    if pat[0] == '*' {
        for i in 0..=txt.len() {
            if i > 0 && txt[i - 1] == '/' { break; }
            if glob_match_impl(&pat[1..], &txt[i..]) { return true; }
        }
        return false;
    }
    if pat[0] == '?' {
        return !txt.is_empty() && txt[0] != '/' && glob_match_impl(&pat[1..], &txt[1..]);
    }
    if pat[0] == '{' {
        if let Some(end) = pat.iter().position(|&c| c == '}') {
            let alts: Vec<&[char]> = pat[1..end].split(|&c| c == ',').collect();
            for alt in alts {
                if txt.starts_with(alt) && glob_match_impl(&pat[end+1..], &txt[alt.len()..]) {
                    return true;
                }
            }
            return false;
        }
    }
    if !txt.is_empty() && (pat[0] == txt[0] || pat[0] == '?') {
        return glob_match_impl(&pat[1..], &txt[1..]);
    }
    false
}

/// The directory git commands run in: the repository's top level once a status
/// refresh has found it (status paths are relative to it), else the workspace root.
fn git_root(s: &State) -> Option<VPath> {
    s.git_panel.repo_root.clone()
        .or_else(|| s.explorer.as_ref().map(|e| e.root.clone()))
        .or_else(|| std::env::current_dir().ok().map(VPath::Local))
}

/// Run `git <args>` in the repository (locally or over SSH) on a background
/// thread, then refresh the panel via `GitOpDone`.
fn run_git_op(s: &mut State, args: Vec<String>) {
    let Some(root) = git_root(s) else { return };
    let proxy = s.proxy.clone();
    std::thread::spawn(move || {
        let error = match &root {
            VPath::Local(dir) => match os::command("git").args(&args).current_dir(dir).stdin(std::process::Stdio::null()).output() {
                Ok(o) if o.status.success() => None,
                Ok(o) => Some(String::from_utf8_lossy(&o.stderr).trim().to_owned()),
                Err(e) => Some(e.to_string()),
            },
            VPath::Remote { host, path } => {
                let quoted: Vec<String> = args.iter().map(|a| ssh::shell_quote(a)).collect();
                let cmd = format!("cd {} && git {}", remote_dir_expr(path), quoted.join(" "));
                ssh::run_ssh_capture(host, &["sh", "-c", &cmd]).err()
            }
        };
        if let Some(e) = error.filter(|e| !e.is_empty()) {
            let _ = proxy.send_event(UserEvent::GitOpFailed { msg: e });
        }
        let _ = proxy.send_event(UserEvent::GitOpDone);
    });
}

/// Shell expression for a remote directory (keeps a leading `~` expandable).
fn remote_dir_expr(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy();
    if raw == "~" { "\"$HOME\"".to_owned() }
    else if let Some(rest) = raw.strip_prefix("~/") { format!("\"$HOME\"/{}", ssh::shell_quote(rest)) }
    else { ssh::shell_quote(&raw) }
}

/// Parse `git status --porcelain=v1 -z` output. NUL-separated records are never
/// quoted, so names with spaces or non-ASCII characters come through intact; a
/// rename/copy record is followed by its original path, which is skipped.
fn parse_porcelain_z(out: &str) -> (Vec<GitEntry>, Vec<GitEntry>) {
    let mut staged = vec![];
    let mut unstaged = vec![];
    let mut records = out.split('\0');
    while let Some(rec) = records.next() {
        if rec.len() < 4 { continue; }
        let mut chars = rec.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        let path = rec[3..].to_owned();
        if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') { records.next(); }
        if x != ' ' && x != '?' {
            staged.push(GitEntry { xy: (x, y), path: path.clone() });
        }
        if (y != ' ' && y != '?') || (x == '?' && y == '?') {
            unstaged.push(GitEntry { xy: (x, y), path });
        }
    }
    (staged, unstaged)
}

/// Route git status refresh to local or remote implementation based on workspace root.
fn trigger_git_status_refresh(s: &mut State) {
    let proxy = s.proxy.clone();
    let root = s.explorer.as_ref().map(|e| e.root.clone())
        .unwrap_or_else(|| VPath::Local(std::env::current_dir().unwrap_or_default()));
    refresh_git_status(proxy, root);
}

fn refresh_git_status(proxy: winit::event_loop::EventLoopProxy<UserEvent>, root: VPath) {
    std::thread::spawn(move || {
        // Print the repository top level first, then the status from there (status
        // paths are relative to the top level, not to a subdirectory we opened).
        const SCRIPT: &str = "top=$(git rev-parse --show-toplevel) || exit 128\nprintf '%s\\0' \"$top\"\ncd \"$top\" && git status --porcelain=v1 -z";
        let (output, error) = match &root {
            VPath::Local(dir) => local_git_status(dir),
            VPath::Remote { host, path } => {
                let cmd = format!("cd {} || exit 1\n{SCRIPT}", remote_dir_expr(path));
                match ssh::run_ssh_capture(host, &["sh", "-c", &cmd]) {
                    Ok(out) => (Some(out), None),
                    Err(e) if e.contains("128") || e.contains("not a git repository") => (None, None),
                    Err(e) => (None, Some(e)),
                }
            }
        };
        let event = match output {
            Some(out) => {
                let (top, rest) = out.split_once('\0').unwrap_or((out.as_str(), ""));
                let repo_root = match &root {
                    VPath::Local(_) => VPath::Local(PathBuf::from(top)),
                    VPath::Remote { host, .. } => VPath::Remote { host: host.clone(), path: PathBuf::from(top) },
                };
                let (staged, unstaged) = parse_porcelain_z(rest);
                UserEvent::GitStatusResult { staged, unstaged, is_git_repo: true, error: None, repo_root: Some(repo_root) }
            }
            None => UserEvent::GitStatusResult { staged: vec![], unstaged: vec![], is_git_repo: error.is_some(), error, repo_root: None },
        };
        let _ = proxy.send_event(event);
    });
}

/// Local equivalent of the status SCRIPT above, without needing `sh` (Windows):
/// "<toplevel>\0<porcelain -z status>", or (None, None) outside a repository.
fn local_git_status(dir: &std::path::Path) -> (Option<String>, Option<String>) {
    let git = |args: &[&str], cwd: &std::path::Path| {
        os::command("git").args(args).current_dir(cwd).stdin(std::process::Stdio::null()).output()
    };
    let top = match git(&["rev-parse", "--show-toplevel"], dir) {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim_end_matches(['\r', '\n']).to_owned(),
        Ok(_) => return (None, None), // not a git repository
        Err(e) => return (None, Some(e.to_string())),
    };
    // git prints `C:/x` on Windows; make it a native path for comparisons.
    let top = if cfg!(windows) { top.replace('/', "\\") } else { top };
    match git(&["status", "--porcelain=v1", "-z"], std::path::Path::new(&top)) {
        Ok(o) if o.status.success() => (Some(format!("{top}\0{}", String::from_utf8_lossy(&o.stdout))), None),
        Ok(o) => (None, Some(String::from_utf8_lossy(&o.stderr).trim().to_owned())),
        Err(e) => (None, Some(e.to_string())),
    }
}

fn refresh_git_diff(
    proxy:  winit::event_loop::EventLoopProxy<UserEvent>,
    root:   VPath,
    path:   String,
    staged: bool,
    buf_id: usize,
) {
    std::thread::spawn(move || {
        let mut args: Vec<&str> = vec!["diff"];
        if staged { args.push("--cached"); }
        args.push("--");
        args.push(&path);
        let text = match &root {
            VPath::Local(dir) => os::command("git")
                .args(&args)
                .current_dir(dir)
                .stdin(std::process::Stdio::null())
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default(),
            VPath::Remote { host, path: dir } => {
                let quoted: Vec<String> = args.iter().map(|a| ssh::shell_quote(a)).collect();
                let cmd = format!("cd {} && git {}", remote_dir_expr(dir), quoted.join(" "));
                ssh::run_ssh_capture(host, &["sh", "-c", &cmd]).unwrap_or_default()
            }
        };
        let lines: Vec<DiffLine> = text.lines().filter_map(|l| {
            if l.starts_with('+') && !l.starts_with("+++") {
                Some(DiffLine::Added(l[1..].to_owned()))
            } else if l.starts_with('-') && !l.starts_with("---") {
                Some(DiffLine::Removed(l[1..].to_owned()))
            } else if l.starts_with("@@") {
                Some(DiffLine::Hunk(l.to_owned()))
            } else if l.starts_with("diff ") || l.starts_with("--- ")
                || l.starts_with("+++ ") || l.starts_with("index ")
                || l.starts_with("Binary ")
            {
                None
            } else {
                Some(DiffLine::Context(l.to_owned()))
            }
        }).collect();
        let _ = proxy.send_event(UserEvent::GitDiffResult { buf_id, path, lines });
    });
}

fn parse_hunk_header(s: &str) -> Option<(usize, usize)> {
    // Parses "@@ -old_start[,count] +new_start[,count] @@..."
    let inner = s.trim_start_matches('@').trim();
    let old_part = inner.split_whitespace().find(|p| p.starts_with('-'))?;
    let new_part = inner.split_whitespace().find(|p| p.starts_with('+'))?;
    let old: usize = old_part.trim_start_matches('-').split(',').next()?.parse().ok()?;
    let new: usize = new_part.trim_start_matches('+').split(',').next()?.parse().ok()?;
    Some((old, new))
}

fn compute_line_nums_at(lines: &[DiffLine], skip: usize) -> (usize, usize) {
    let mut old = 0usize;
    let mut new = 0usize;
    for line in lines.iter().take(skip) {
        match line {
            DiffLine::Hunk(h) => { if let Some((o, n)) = parse_hunk_header(h) { old = o; new = n; } }
            DiffLine::Added(_)   => { new += 1; }
            DiffLine::Removed(_) => { old += 1; }
            DiffLine::Context(_) => { old += 1; new += 1; }
        }
    }
    (old, new)
}

fn open_diff_tab(s: &mut State, path: String, staged: bool) {
    // Find target editor pane, creating one if every editor tab has been closed.
    let ap = ensure_editor_pane(s);
    // Check if a GitDiff tab for this path already exists in the pane
    {
        let pane = s.panes.get(&ap).unwrap();
        for i in 0..pane.tabs.len() {
            let t = &pane.tabs[i];
            if t.kind == TabKind::GitDiff {
                if s.git_diff_tabs.get(&t.buf_id).map_or(false, |d| d.path == path) {
                    s.panes.get_mut(&ap).unwrap().active = i;
                    return;
                }
            }
        }
    }
    // Create new GitDiff tab
    let Some(root) = git_root(s) else { return };
    let buf_id = s.next_buf_id;
    s.next_buf_id += 1;
    let mut tab = Tab::untitled(buf_id);
    tab.kind = TabKind::GitDiff;
    // Absolute path of the diffed file (display only — diff tabs are read-only and
    // can never be saved, see `Tab::save_with_proxy`).
    tab.path = Some(root.join(&path));
    s.git_diff_tabs.insert(buf_id, GitDiffTabData { path: path.clone(), staged, lines: vec![], loading: true });
    let pane = s.panes.get_mut(&ap).unwrap();
    // Reuse a blank untitled tab (e.g. the one in a freshly-created pane) instead
    // of leaving it stranded beside the diff.
    if pane.tab().is_empty_untitled() && !pane.tab().dirty {
        let active = pane.active;
        pane.tabs[active] = tab;
    } else {
        pane.tabs.push(tab);
        pane.active = pane.tabs.len() - 1;
    }
    refresh_git_diff(s.proxy.clone(), root, path, staged, buf_id);
}

fn spawn_search(gf: &mut GlobalFind, root: PathBuf, proxy: EventLoopProxy<UserEvent>) {
    let query   = gf.query.clone();
    let include = gf.include_glob.clone();
    let exclude = gf.exclude_glob.clone();
    let cs      = gf.case_sensitive;
    gf.search_token += 1;
    let token = gf.search_token;
    gf.searching = true;
    gf.results.clear();
    gf.selected = 0;
    gf.scroll   = 0;
    std::thread::spawn(move || {
        let results = if query.is_empty() { Vec::new() }
                      else { global_search(&root, &query, &include, &exclude, cs) };
        let _ = proxy.send_event(UserEvent::SearchDone { token, results });
    });
}

fn global_search(root: &std::path::Path, query: &str, include: &str, exclude: &str, case_sensitive: bool) -> Vec<GlobalFindResult> {
    let mut all_files = Vec::new();
    walk_files(root, &mut all_files, 0);
    let (query_chars, _) = fold_chars(query.chars(), case_sensitive);
    let mut results = Vec::new();
    for local_path in &all_files {
        let rel = local_path.strip_prefix(root).unwrap_or(local_path).to_string_lossy().to_string();
        if !include.is_empty() && !glob_match(include, &rel) { continue; }
        if !exclude.is_empty() && glob_match(exclude, &rel) { continue; }
        if std::fs::metadata(local_path).map(|m| m.len()).unwrap_or(0) > 64 * 1024 * 1024 { continue; }
        let Ok(content) = std::fs::read_to_string(local_path) else { continue };
        let path = VPath::Local(local_path.clone());
        for (line_num, line) in content.lines().enumerate() {
            let line_chars: Vec<char> = line.chars().collect();
            for (lo, hi) in find_in_chars(&line_chars, &query_chars, case_sensitive) {
                results.push(GlobalFindResult {
                    path:      path.clone(),
                    line_num,
                    line_text: line.chars().take(200).collect(),
                    match_col: lo,          // char column (what the editor jumps to)
                    match_len: hi - lo,
                });
                if results.len() >= 1000 { return results; }
            }
        }
    }
    results
}

/// Replace every match of `query` in `content`, line by line, keeping each line's
/// own ending (`\n` or `\r\n`) and whether the file ends with a newline. Returns
/// the new text and the number of replacements (None if nothing matched).
fn replace_in_text(content: &str, query: &str, replace: &str, case_sensitive: bool) -> Option<(String, usize)> {
    let (query_chars, _) = fold_chars(query.chars(), case_sensitive);
    let mut out = String::with_capacity(content.len());
    let mut count = 0;
    for line in content.split_inclusive('\n') {
        let (body, ending) = if let Some(b) = line.strip_suffix("\r\n") { (b, "\r\n") }
            else if let Some(b) = line.strip_suffix('\n') { (b, "\n") }
            else { (line, "") };
        let chars: Vec<char> = body.chars().collect();
        let ms = find_in_chars(&chars, &query_chars, case_sensitive);
        if ms.is_empty() {
            out.push_str(line);
            continue;
        }
        let mut last = 0;
        for (lo, hi) in ms {
            out.extend(chars[last..lo].iter());
            out.push_str(replace);
            last = hi;
            count += 1;
        }
        out.extend(chars[last..].iter());
        out.push_str(ending);
    }
    (count > 0).then_some((out, count))
}

/// Replace All across the files in the search results. Matches are found again in
/// each file's *current* contents (results can be stale). Files open in a tab are
/// edited in the buffer — undoable, left unsaved — so unsaved edits are never
/// overwritten; other files are rewritten atomically on disk.
fn global_replace(s: &mut State, results: &[GlobalFindResult], query: &str, replace: &str, case_sensitive: bool) {
    let mut paths: Vec<VPath> = Vec::new();
    for r in results {
        if r.path.as_local_path().is_some() && !paths.contains(&r.path) { paths.push(r.path.clone()); }
    }
    let limit = s.settings.undo_limit;
    let (mut files, mut total, mut errors) = (0usize, 0usize, Vec::new());
    for path in paths {
        if let Some((pid, ti)) = find_tab_for_path(s, &path) {
            let collab = s.collab.is_some();
            let tab = &mut s.panes.get_mut(&pid).unwrap().tabs[ti];
            let ms = find_matches(&tab.text, query, case_sensitive, false);
            if ms.is_empty() { continue; }
            let before = tab.text.clone();
            if !collab { tab.push_undo_snapshot(limit); }
            tab.note_edit();
            let n = ms.len();
            let edits = ms.into_iter().map(|(lo, hi)| (lo, hi, replace.to_owned())).collect();
            let mut cursors = std::mem::take(&mut tab.cursors);
            apply_char_edits(&mut tab.text, &mut cursors, edits);
            tab.cursors = cursors;
            tab.clamp_cursors();
            tab.dirty = true;
            after_background_edit(s, pid, ti, Some(before));
            files += 1;
            total += n;
        } else if let Some(local) = path.as_local_path() {
            let Ok(content) = std::fs::read_to_string(local) else {
                errors.push(format!("{}: unreadable", local.display()));
                continue;
            };
            let Some((new_content, n)) = replace_in_text(&content, query, replace, case_sensitive) else { continue };
            match write_file_atomically(local, new_content.as_bytes()) {
                Ok(()) => { files += 1; total += n; }
                Err(e) => errors.push(format!("{}: {e}", local.display())),
            }
        }
    }
    s.status_msg = Some(if errors.is_empty() {
        format!("Replaced {total} occurrence{} in {files} file{}", if total == 1 { "" } else { "s" }, if files == 1 { "" } else { "s" })
    } else {
        format!("Replaced {total} in {files} files; failed: {}", errors.join("; "))
    });
}

fn darken_buffer(buf: &mut [u32], w: u32, h: u32) {
    // Halve each channel with a single shift+mask — no per-channel extraction needed.
    // 0xFEFEFEFE mask strips the LSB of each byte before shifting so no channel bleeds
    // into its neighbour. This is a single two-instruction body that LLVM auto-vectorizes
    // into NEON ushrl.4s on Apple Silicon (4 pixels per instruction).
    let len = (w as usize).saturating_mul(h as usize).min(buf.len());
    for p in buf[..len].iter_mut() {
        *p = (*p & 0xFEFEFEFE) >> 1;
    }
}

// ── App ───────────────────────────────────────────────────────────────────────
struct App {
    state:        Option<State>,
    file_arg:     Option<VPath>,
    dir_arg:      Option<VPath>,
    proxy:        EventLoopProxy<UserEvent>,
    dirty:        Arc<AtomicBool>,
    display_link: Option<platform::DisplayLink>,
    /// Control socket for cross-process LSP coordination; Drop unlinks it.
    ctl_listener: Option<lsp_registry::ControlListener>,
}

impl App {
    fn new(file_arg: Option<VPath>, dir_arg: Option<VPath>, proxy: EventLoopProxy<UserEvent>) -> Self {
        Self { state: None, file_arg, dir_arg, proxy, dirty: Arc::new(AtomicBool::new(false)), display_link: None,
               ctl_listener: None }
    }

    fn apply_vsync_setting(&mut self) {
        let vsync = self.state.as_ref().map_or(false, |s| s.settings.vsync);
        if vsync {
            if self.display_link.is_none() {
                let proxy  = self.proxy.clone();
                let dirty2 = Arc::clone(&self.dirty);
                self.display_link = platform::DisplayLink::new(move || {
                    if dirty2.swap(false, Ordering::AcqRel) {
                        let _ = proxy.send_event(UserEvent::Redraw);
                    }
                });
            }
        } else {
            self.display_link = None;
        }
    }
}

impl App {
    fn handle_new_events(&mut self, _el: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            let now = Instant::now();
            if let Some(s) = self.state.as_mut() {
                // Cursor blink
                if now >= s.cursor_blink {
                    dlog!("[blink] t={}", ts());
                    s.cursor_visible = !s.cursor_visible;
                    s.cursor_blink = now + Duration::from_millis(500);
                    self.dirty.store(true, Ordering::Release);
                    s.needs_redraw = true;
                }
                // Poll only the root and expanded folders. Remote requests are
                // deduplicated so a slow SSH round-trip cannot accumulate threads.
                if s.left_panel_visible && s.left_view == LeftView::FileTree {
                    let mut remote_refreshes = Vec::new();
                    let refreshed = s.explorer.as_mut().map_or(false, |explorer| {
                        let interval = if explorer.root.is_remote() {
                            Duration::from_secs(1)
                        } else {
                            Duration::from_millis(500)
                        };
                        if now.duration_since(explorer.last_refresh) < interval { return false; }
                        explorer.last_refresh = now;
                        match explorer.root.clone() {
                            VPath::Local(_) => explorer.refresh_local(),
                            VPath::Remote { host, path } => {
                                let paths = std::iter::once(path)
                                    .chain(explorer.entries.iter()
                                        .filter(|entry| entry.is_dir && entry.expanded)
                                        .map(|entry| entry.path.clone()))
                                    .collect::<Vec<_>>();
                                for path in paths {
                                    if explorer.pending_remote_listings.insert(path.clone()) {
                                        remote_refreshes.push((host.clone(), path));
                                    }
                                }
                                false
                            }
                        }
                    });
                    for (host, path) in remote_refreshes {
                        ssh::ssh_list_dir(host, path, s.proxy.clone());
                    }
                    if let Some(ex) = s.explorer.as_mut() {
                        if ex.tree_search_rewalk && !ex.tree_search_walking {
                            spawn_tree_search_walk(ex, &s.proxy);
                        }
                    }
                    if refreshed {
                        self.dirty.store(true, Ordering::Release);
                        s.needs_redraw = true;
                    }
                }
                // Live search debounce
                if let Some(fire_at) = s.global_find.search_fire_at {
                    if now >= fire_at {
                        s.global_find.search_fire_at = None;
                        let root = s.explorer.as_ref()
                            .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                        spawn_search(&mut s.global_find, root, s.proxy.clone());
                        self.dirty.store(true, Ordering::Release);
                        s.needs_redraw = true;
                    }
                }
            }
        }
    }

    fn handle_resumed(&mut self, el: &ActiveEventLoop) {
        let attrs = WindowAttributes::default().with_title("local-text");
        #[cfg(target_os = "macos")]
        let attrs = attrs.with_title_hidden(true).with_titlebar_transparent(true);

        let loaded_settings = settings::Settings::load();

        let win = Arc::new(el.create_window(attrs).unwrap());
        let renderer = match loaded_settings.renderer {
            settings::RendererBackend::Gpu => platform::Renderer::new_gpu(&win, loaded_settings.gpu_drawable_count),
            settings::RendererBackend::Cpu => platform::Renderer::new_cpu(&win, loaded_settings.cpu_double_buffer),
        };

        let font_size = loaded_settings.font_size;
        let mut glyphs = Glyphs::new(include_bytes!("../assets/JetBrainsMono-Regular.ttf"), font_size);
        glyphs.max_entries = loaded_settings.glyph_cache_limit.cap();

        let mut initial_pane = Pane::new(0, 0); // pane 0, buf 0
        let mut startup_status: Option<String> = None;
        let file_arg = self.file_arg.take();
        let dir_arg  = self.dir_arg.take();

        // Kick off SSH ControlMaster for any remote path before trying to open it.
        let remote_host = file_arg.as_ref().and_then(|p| p.ssh_host().cloned())
            .or_else(|| dir_arg.as_ref().and_then(|p| p.ssh_host().cloned()));
        if let Some(host) = remote_host {
            ssh::ensure_control_master(host, self.proxy.clone());
        }

        let mut initial_lsp_path: Option<VPath> = None;
        if let Some(path) = file_arg {
            match initial_pane.tabs[0].load_file(path.clone()) {
                Ok(()) => initial_lsp_path = Some(path.clone()),
                // Remote files: async load triggered via ensure_control_master → SshConnected.
                // The tab has path set but empty text; RemoteFileContent will fill it in.
                Err(LoadError::Remote) => {}
                // `local-text new.rs` starts a new file that the first save creates.
                Err(LoadError::NotFound) => {
                    initial_pane.tabs[0].start_new_file(path.clone());
                    startup_status = Some(format!("New file: {path}"));
                    initial_lsp_path = Some(path.clone());
                }
                Err(e) => startup_status = Some(e.message(&path)),
            }
        }
        if startup_status.is_none() {
            if let Some(base) = recovery_base_dir() {
                if std::fs::read_dir(&base).map_or(false, |mut d| d.next().is_some()) {
                    startup_status = Some(format!("Unsaved files from an earlier session were saved in {}", base.display()));
                }
            }
        }
        let mut panes = HashMap::new();
        panes.insert(0usize, initial_pane);

        let explorer = dir_arg.map(FileExplorer::new);
        let sz = win.inner_size();

        let mut s = State {
            win,
            renderer,
            w: sz.width,
            h: sz.height,
            pane_tree:    PaneTree::Leaf(0),
            panes,
            active_pane:  0,
            next_pane_id: 1,
            next_buf_id:  1,
            next_remote_read_token: 1,
            drag:         None,
            drag_pending: None,
            resize_drag:  None,
            term_panes:  HashMap::new(),
            md_panes:    HashMap::new(),
            lsp:           lsp::LspManager::new(),
            diagnostics:   HashMap::new(),
            lsp_installed: HashMap::new(),
            remote_lsp_installed: HashMap::new(),
            remote_lsp_check_errors: HashMap::new(),
            lsp_start_errors: HashMap::new(),
            proxy:         self.proxy.clone(),
            cursor_visible: true,
            cursor_blink:   Instant::now() + Duration::from_millis(500),
            mods:   ModifiersState::default(),
            font_size,
            glyphs,
            explorer,
            explorer_w:    ((200.0 * font_size / FONT_PX).round() as i32).clamp(80, 600),
            explorer_drag: false,
            mouse_x:    0.0,
            mouse_y:    0.0,
            mouse_down:       false,
            term_buttons_held: 0,
            term_sel:        None,
            term_selecting:  false,
            term_click_count:     0,
            term_last_click_time: Instant::now() - Duration::from_secs(1),
            term_last_click_row:  0,
            term_last_click_col:  0,
            last_click_time: Instant::now() - Duration::from_secs(1),
            last_click_char: usize::MAX,
            click_count:     0,

            settings:     loaded_settings,
            needs_redraw: false,

            settings_edit_field:  None,
            settings_edit_text:   String::new(),
            settings_edit_cursor: 0,

            cursor_back: Vec::new(),
            cursor_fwd:  Vec::new(),

            left_view:          LeftView::FileTree,
            left_panel_visible: true,
            sidebar_list_focused: false,
            diag_panel_sel: 0,
            context_menu: None,
            explorer_clipboard: None,
            lsp_panel:    None,
            quick_finder: QuickFinder {
                open: false, query: String::new(), cursor: 0, sel_anchor: None,
                entries: vec![], filtered: vec![], filtered_commands: vec![], selected: 0,
                walk_token: 0, loading: false,
                remote_directory_mode: false,
                collab_mode: false,
                parsed_line: None, parsed_col: None, direct_path: None,
                preview_pane_id: None, preview_tab_idx: None, preview_saved_char: None,
            },
            global_find: GlobalFind {
                query: String::new(), replace: String::new(),
                include_glob: String::new(), exclude_glob: String::new(),
                results: vec![], scroll: 0, selected: 0,
                focus: GlobalFindFocus::Query, case_sensitive: false,
                live_search: false, search_fire_at: None,
                searching: false, search_token: 0,
                cursor_query: 0, cursor_replace: 0, cursor_include: 0, cursor_exclude: 0,
                sel_anchor_q: None, sel_anchor_r: None, sel_anchor_inc: None, sel_anchor_exc: None,
            },
            git_panel: GitPanel::new(),
            git_diff_tabs: HashMap::new(),
            scroll_frac_y: 0.0,
            status_msg: startup_status,

            last_sync_pane: usize::MAX,
            last_sync_tab:  usize::MAX,
            last_sync_gen:  u64::MAX,

            ssh_connections: HashMap::new(),

            collab:           None,
            collab_before:    None,
            discard_armed:    None,
            collab_file_list: Vec::new(),
        };

        if let Some(path) = initial_lsp_path {
            notify_lsp_open(&mut s, &path);
        }
        if let Some(root) = s.explorer.as_ref().map(|ex| ex.root.clone()) {
            start_lsp_for_project(&mut s, &root);
        }
        s.win.request_redraw();
        self.state = Some(s);
        self.apply_vsync_setting();

        // Cross-process LSP coordination: bind our control socket so other
        // local-text windows can query/stop our servers and raise our window.
        if self.ctl_listener.is_none() {
            self.ctl_listener = lsp_registry::start_control_listener(self.proxy.clone());
        }
    }

    fn handle_exiting(&mut self, _el: &ActiveEventLoop) {
        // Exits that bypass our close prompt (e.g. Cmd+Q from the macOS app menu)
        // still must not lose work: dump unsaved buffers to the recovery folder.
        if let Some(s) = self.state.as_ref() {
            write_recovery_files(s);
        }
        // Best-effort: remove our registry entries + control socket. The liveness
        // GC in lsp_registry is the backstop for unclean exits (SIGKILL/panic).
        lsp_registry::cleanup_self();
        self.ctl_listener = None;
    }

    fn handle_window_event(&mut self, el: &ActiveEventLoop, _id: winit::window::WindowId, event: WindowEvent) {
        let Some(s) = self.state.as_mut() else { return };
        if s.panes.is_empty() { return; }
        s.ensure_active_pane(); // re-point active_pane if a prior handler left it dangling

        match event {
            WindowEvent::CloseRequested => {
                if request_quit(s) {
                    s.panes.clear();
                    el.exit();
                }
                s.needs_redraw = true;
            }

            WindowEvent::Focused(_focused) => {
                dlog!("[focus] focused={_focused} t={}", ts());
            }

            WindowEvent::Resized(sz) => {
                dlog!("[resize] {}x{} -> {}x{}", s.w, s.h, sz.width, sz.height);
                s.w = sz.width;
                s.h = sz.height;
                s.renderer.resize(sz.width, sz.height);
                resize_terminal_panes(s);
                { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
            }

            WindowEvent::ModifiersChanged(m) => {
                s.mods = m.state();
                // Redraw so cmd+hover underline appears/disappears immediately
                { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
            }

            WindowEvent::CursorMoved { position, .. } => {
                s.mouse_x = position.x as f32;
                s.mouse_y = position.y as f32;
                let mx = s.mouse_x as i32;
                let my = s.mouse_y as i32;

                // Explorer border drag
                if s.explorer_drag {
                    let act_w = s.activity_bar_w();
                    s.explorer_w = (mx - act_w).clamp(80, 600);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Update context menu hover
                if let Some(ref mut cm) = s.context_menu {
                    let item_h = s.glyphs.lh + 2;
                    let sep_h  = 5i32;
                    let label_w = cm.items.iter().filter(|i| i.action != CtxAction::Separator).map(|i| i.label.chars().count()).max().unwrap_or(8) as i32;
                    let sc_w    = cm.items.iter().filter(|i| i.action != CtxAction::Separator).map(|i| key_label(i.shortcut).chars().count()).max().unwrap_or(0) as i32;
                    let menu_w  = (label_w + sc_w + 4) * s.glyphs.cw + 16;
                    let total_h: i32 = cm.items.iter().map(|i| if i.action == CtxAction::Separator { sep_h } else { item_h }).sum::<i32>() + 4;
                    let menu_x = cm.x.min(s.w as i32 - menu_w).max(0);
                    let menu_y = cm.y.min(s.h as i32 - total_h).max(0);
                    if mx >= menu_x && mx < menu_x + menu_w && my >= menu_y && my < menu_y + total_h {
                        // Find which item is hovered by scanning y positions
                        let mut iy = menu_y + 2;
                        for (idx, item) in cm.items.iter().enumerate() {
                            let ih = if item.action == CtxAction::Separator { sep_h } else { item_h };
                            if my >= iy && my < iy + ih { cm.hovered = idx; break; }
                            iy += ih;
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                }

                // Active pane-resize drag
                if let Some(ref rd) = s.resize_drag {
                    let new_ratio = match rd.axis {
                        Axis::H => ((mx - rd.rect.x) as f32 / rd.rect.w as f32).clamp(0.05, 0.95),
                        Axis::V => ((my - rd.rect.y) as f32 / rd.rect.h as f32).clamp(0.05, 0.95),
                    };
                    let path = rd.path.clone();
                    let axis = rd.axis;
                    update_ratio(&mut s.pane_tree, &path, new_ratio);
                    resize_terminal_panes(s);
                    s.win.set_cursor(if axis == Axis::H { CursorIcon::EwResize } else { CursorIcon::NsResize });
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Active tab drag: update over_pane and zone
                if s.drag.is_some() {
                    let area = s.pane_area();
                    let over = pane_at_pos(&s.pane_tree, mx, my, area);
                    let zone = over.map(|pid| {
                        let rect = layout_tree(&s.pane_tree, area).into_iter()
                            .find(|(id, _)| *id == pid).map(|(_, r)| r).unwrap_or(area);
                        drop_zone(mx, my, rect, s.tab_h())
                    });
                    if let Some(ref mut drag) = s.drag {
                        drag.cur_x = s.mouse_x;
                        drag.cur_y = s.mouse_y;
                        drag.over_pane = over;
                        drag.zone = zone;
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Promote drag_pending to drag if moved >5px
                if let Some((pane_id, tab_idx, sx, sy)) = s.drag_pending {
                    let dx = mx as f32 - sx;
                    let dy = my as f32 - sy;
                    if dx * dx + dy * dy > 25.0 {
                        let area = s.pane_area();
                        let over = pane_at_pos(&s.pane_tree, mx, my, area);
                        s.drag = Some(DragState {
                            source_pane: pane_id, source_tab: tab_idx,
                            cur_x: s.mouse_x, cur_y: s.mouse_y,
                            over_pane: over, zone: None,
                        });
                        s.drag_pending = None;
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                }

                // Cursor icon: border hover → resize, editor area → text, else default
                let act_w_c = s.activity_bar_w();
                let ew_c = s.explorer_w();
                // Wide hit zone: 8px inside panel + 4px outside, so the cursor change
                // is clearly visible when approaching the border from either side.
                let near_explorer_border = s.explorer.is_some() && ew_c > 0
                    && mx >= act_w_c + ew_c - 8
                    && mx <= act_w_c + ew_c + 4
                    && my < s.h as i32 - s.status_h();
                if near_explorer_border {
                    s.win.set_cursor(CursorIcon::EwResize);
                } else {
                    let area = s.pane_area();
                    let border = if mx >= area.x && my >= area.y && my < area.y + area.h {
                        find_border_at(&s.pane_tree, area, mx, my)
                    } else {
                        None
                    };
                    if let Some((_, axis, _)) = border {
                        s.win.set_cursor(if axis == Axis::H { CursorIcon::EwResize } else { CursorIcon::NsResize });
                    } else {
                        let r     = s.active_pane_rect();
                        let fh    = s.find_h();
                        let in_ed = my >= r.y + s.tab_h() && my < r.y + r.h - fh && mx >= r.x;
                        let cmd   = primary_mod(s.mods);
                        // When Cmd (Linux: Ctrl) is held over the editor content, use the pointer
                        // cursor to signal that Cmd/Ctrl+Click will navigate (goto definition)
                        let cursor = if !in_ed {
                            CursorIcon::Default
                        } else if cmd {
                            CursorIcon::Pointer
                        } else {
                            CursorIcon::Text
                        };
                        s.win.set_cursor(cursor);
                    }
                }
                if s.mouse_down
                    && s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Editor && !p.tabs.is_empty())
                {
                    let pos = s.xy_to_char(mx, my);
                    s.tab_mut().primary_mut().head = pos;
                    s.ensure_visible();
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                }
                // Redraw for hover tooltip updates
                { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }

                // Terminal mouse motion: selection drag + mouse reporting
                {
                    let pane_id = s.active_pane;
                    if s.panes.get(&pane_id).map_or(false, |p| p.kind == PaneKind::Terminal) {
                        let tid = s.panes[&pane_id].term_ids.get(s.panes[&pane_id].active).copied();
                        if let Some(tid) = tid {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                // Selection drag
                                if s.term_selecting {
                                    let area = s.pane_area();
                                    let pane_rect = layout_tree(&s.pane_tree, area).into_iter()
                                        .find(|(id, _)| *id == pane_id).map(|(_, r)| r).unwrap_or(area);
                                    let tab_h = s.tab_h();
                                    let content_y = pane_rect.y + tab_h;
                                    let cw = s.glyphs.cw;
                                    let lh = s.glyphs.lh;
                                    let term_col = ((mx - pane_rect.x) / cw)
                                        .clamp(0, tp.grid.cols as i32 - 1) as usize;
                                    let term_row = ((my - content_y) / lh)
                                        .clamp(0, tp.grid.rows as i32 - 1) as usize;
                                    if let Some(ref mut sel) = s.term_sel {
                                        sel.end_vi  = term_row;
                                        sel.end_col = term_col;
                                    }
                                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                                }

                                let should_report = match tp.grid.mouse_report {
                                    terminal::MouseReportMode::AnyEvent    => true,
                                    terminal::MouseReportMode::ButtonEvent => s.term_buttons_held != 0,
                                    _ => false,
                                };
                                if should_report {
                                    let area = s.pane_area();
                                    let pane_rect = layout_tree(&s.pane_tree, area).into_iter()
                                        .find(|(id, _)| *id == pane_id).map(|(_, r)| r).unwrap_or(area);
                                    let tab_h = s.tab_h();
                                    let content_y = pane_rect.y + tab_h;
                                    let cw = s.glyphs.cw;
                                    let lh = s.glyphs.lh;
                                    let term_col = ((mx - pane_rect.x) / cw)
                                        .clamp(0, tp.grid.cols as i32 - 1) as usize;
                                    let term_row = ((my - content_y) / lh)
                                        .clamp(0, tp.grid.rows as i32 - 1) as usize;
                                    let mut mod_bits: u8 = 0;
                                    if s.mods.shift_key()   { mod_bits |= 4; }
                                    if s.mods.alt_key()     { mod_bits |= 8; }
                                    if s.mods.control_key() { mod_bits |= 16; }
                                    // 32+button for drag, 35 (32+3) for no-button motion
                                    let cb = if s.term_buttons_held & 1 != 0 { 32u8 | mod_bits }
                                             else { 35u8 | mod_bits };
                                    let sgr = tp.grid.mouse_sgr;
                                    let pty = tp.input();
                                    let bytes = terminal::encode_mouse(term_col, term_row, cb, true, sgr);
                                    pty.write(&bytes);
                                }
                            }
                        }
                    }
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left, ..
            } => {
                if s.explorer_drag {
                    s.explorer_drag = false;
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }
                if s.resize_drag.take().is_some() {
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }
                s.mouse_down = false;
                s.term_selecting = false;
                if s.term_sel.as_ref().map_or(false, |sel| sel.is_empty()) { s.term_sel = None; }
                s.drag_pending = None;
                if let Some(drag) = s.drag.take() {
                    perform_drop(s, drag);
                    resize_terminal_panes(s); // update PTY size after any layout change
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                }
                // Forward release to terminal if mouse reporting enabled
                if s.term_buttons_held & 1 != 0 {
                    s.term_buttons_held &= !1;
                    let mx = s.mouse_x as i32;
                    let my = s.mouse_y as i32;
                    let pane_id = s.active_pane;
                    if s.panes.get(&pane_id).map_or(false, |p| p.kind == PaneKind::Terminal) {
                        let area = s.pane_area();
                        let pane_rect = layout_tree(&s.pane_tree, area).into_iter()
                            .find(|(id, _)| *id == pane_id).map(|(_, r)| r).unwrap_or(area);
                        let tab_h = s.tab_h();
                        let tid = s.panes[&pane_id].term_ids.get(s.panes[&pane_id].active).copied();
                        if let Some(tid) = tid {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                if tp.grid.mouse_report != terminal::MouseReportMode::None {
                                    let cw = s.glyphs.cw;
                                    let lh = s.glyphs.lh;
                                    let content_y = pane_rect.y + tab_h;
                                    let term_col = ((mx - pane_rect.x) / cw)
                                        .clamp(0, tp.grid.cols as i32 - 1) as usize;
                                    let term_row = ((my - content_y) / lh)
                                        .clamp(0, tp.grid.rows as i32 - 1) as usize;
                                    let mut mod_bits: u8 = 0;
                                    if s.mods.shift_key()   { mod_bits |= 4; }
                                    if s.mods.alt_key()     { mod_bits |= 8; }
                                    if s.mods.control_key() { mod_bits |= 16; }
                                    let sgr = tp.grid.mouse_sgr;
                                    let pty = tp.input();
                                    let bytes = terminal::encode_mouse(term_col, term_row,
                                        if sgr { mod_bits } else { 3 | mod_bits }, false, sgr);
                                    pty.write(&bytes);
                                }
                            }
                        }
                    }
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right, ..
            } => {
                let mx = s.mouse_x as i32;
                let my = s.mouse_y as i32;
                s.context_menu = None;
                let area = s.pane_area();
                let tab_h = s.tab_h();
                let cw = s.glyphs.cw;
                let lh = s.glyphs.lh;
                let layout = layout_tree(&s.pane_tree, area);

                // Git panel entry right-click
                let act_w_r = s.activity_bar_w();
                if s.explorer.is_some() && s.left_view == LeftView::FileTree && s.left_panel_visible
                    && mx >= act_w_r && mx < act_w_r + s.explorer_w()
                {
                    let row = my / lh;
                    let scroll_off = s.explorer.as_ref().map_or(0, |ex| ex.tree_scroll);
                    let idx = row.saturating_sub(2) as usize + scroll_off;
                    let entry = s.explorer.as_ref().map(|ex| {
                        if row >= 2 && ex.tree_search.is_empty() && idx < ex.entries.len() {
                            (explorer_entry_vpath(ex, ex.entries[idx].path.clone()), ex.entries[idx].is_dir, false)
                        } else {
                            (ex.root.clone(), true, true)
                        }
                    });
                    if let Some((entry, is_dir, is_root)) = entry {
                        let can_paste = match (&entry, s.explorer_clipboard.as_ref()) {
                            (VPath::Local(_), Some(VPath::Local(_))) => true,
                            (
                                VPath::Remote { host: destination_host, .. },
                                Some(VPath::Remote { host: source_host, .. }),
                            ) => destination_host == source_host,
                            _ => false,
                        };
                        let mut items = vec![];
                        if is_dir {
                            items.push(ContextMenuItem { label: "New File", shortcut: "", action: CtxAction::ExplorerNewFile, enabled: true });
                            items.push(ContextMenuItem { label: "New Folder", shortcut: "", action: CtxAction::ExplorerNewFolder, enabled: true });
                            items.push(ContextMenuItem { label: "", shortcut: "", action: CtxAction::Separator, enabled: true });
                        }
                        if !is_root {
                            items.push(ContextMenuItem { label: "Copy", shortcut: "", action: CtxAction::ExplorerCopy, enabled: true });
                            items.push(ContextMenuItem { label: "Duplicate", shortcut: "", action: CtxAction::ExplorerDuplicate, enabled: true });
                            items.push(ContextMenuItem { label: "Rename", shortcut: "", action: CtxAction::ExplorerRename, enabled: true });
                        }
                        items.push(ContextMenuItem {
                            label: "Paste",
                            shortcut: "",
                            action: CtxAction::ExplorerPaste,
                            enabled: can_paste,
                        });
                        items.push(ContextMenuItem { label: "", shortcut: "", action: CtxAction::Separator, enabled: true });
                        if is_dir && !is_root {
                            items.push(ContextMenuItem { label: "Open Folder Here", shortcut: "", action: CtxAction::ExplorerOpenAsRoot, enabled: true });
                        }
                        items.push(ContextMenuItem { label: "Open Terminal Here", shortcut: "", action: CtxAction::OpenTerminalHere, enabled: true });
                        s.context_menu = Some(ContextMenu {
                            x: mx, y: my, hovered: 0,
                            tab_source: None, git_entry: None,
                            explorer_entry: Some(entry),
                            explorer_entry_is_dir: is_dir,
                            diag_message: None,
                            items,
                        });
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                }

                if s.explorer.is_some() && s.left_view == LeftView::Git && s.left_panel_visible
                    && mx >= act_w_r && mx < act_w_r + s.explorer_w()
                {
                    let scroll_px       = s.git_panel.scroll as i32 * lh;
                    let staged_header_y = 4 - scroll_px;
                    let staged_start_y  = staged_header_y + lh;
                    let staged_count    = s.git_panel.staged.len();
                    let staged_rows     = staged_count.max(1);
                    let changes_hdr_y   = staged_start_y + staged_rows as i32 * lh + 2;
                    let unstaged_start_y = changes_hdr_y + lh;
                    let panel_h         = s.h as i32 - s.status_h();
                    let commit_area_top = panel_h - lh * 3 - 8;

                    let git_entry_hit: Option<(bool, String)> =
                        if my >= staged_start_y && my < staged_start_y + staged_count as i32 * lh && staged_count > 0 {
                            let i = ((my - staged_start_y) / lh) as usize;
                            s.git_panel.staged.get(i).map(|e| (true, e.path.clone()))
                        } else if my >= unstaged_start_y && my < commit_area_top {
                            let unstaged_count = s.git_panel.unstaged.len();
                            let i = ((my - unstaged_start_y) / lh) as usize;
                            if i < unstaged_count { s.git_panel.unstaged.get(i).map(|e| (false, e.path.clone())) } else { None }
                        } else { None };

                    if let Some((is_staged, entry_path)) = git_entry_hit {
                        let stage_label:  &'static str = if is_staged { "Unstage" } else { "Stage" };
                        let stage_action: CtxAction     = if is_staged { CtxAction::GitUnstage } else { CtxAction::GitStage };
                        s.context_menu = Some(ContextMenu {
                            x: mx, y: my, hovered: 0, tab_source: None,
                            git_entry: Some((is_staged, entry_path)),
                            explorer_entry: None, explorer_entry_is_dir: false,
                            diag_message: None,
                            items: vec![
                                ContextMenuItem { label: "Open File", shortcut: "", action: CtxAction::GitOpenFile, enabled: true },
                                ContextMenuItem { label: "View Diff", shortcut: "", action: CtxAction::GitViewDiff, enabled: true },
                                ContextMenuItem { label: "",          shortcut: "", action: CtxAction::Separator,   enabled: true },
                                ContextMenuItem { label: stage_label, shortcut: "", action: stage_action,           enabled: true },
                            ],
                        });
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Check if click lands in a tab bar — collect hit info without long borrow
                struct TabHit { pid: usize, ti: usize, has_path: bool, is_md: bool }
                let tab_hit: Option<TabHit> = 'find: {
                    for &(pid, rect) in &layout {
                        if !(my >= rect.y && my < rect.y + tab_h) { continue; }
                        let pane = match s.panes.get(&pid) {
                            Some(p) if p.kind == PaneKind::Editor => p,
                            _ => continue,
                        };
                        let mut tx = rect.x;
                        for (ti, tab) in pane.tabs.iter().enumerate() {
                            let label_chars = tab.display_name().chars().count() + if tab.dirty { 4 } else { 3 };
                            let tw = label_chars as i32 * cw + 1;
                            if mx >= tx && mx < tx + tw {
                                let has_path = tab.path.is_some();
                                let is_md = tab.path.as_ref()
                                    .map(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("md" | "markdown")))
                                    .unwrap_or(false);
                                break 'find Some(TabHit { pid, ti, has_path, is_md });
                            }
                            tx += tw;
                        }
                    }
                    None
                };

                if let Some(hit) = tab_hit {
                    s.active_pane = hit.pid;
                    let mut items: Vec<ContextMenuItem> = vec![];
                    if hit.has_path {
                        items.push(ContextMenuItem { label: "Copy Relative Path", shortcut: "",            action: CtxAction::TabCopyRelPath,  enabled: true });
                        items.push(ContextMenuItem { label: "Copy Full Path",     shortcut: "",            action: CtxAction::TabCopyFullPath, enabled: true });
                        items.push(ContextMenuItem { label: "",                   shortcut: "",            action: CtxAction::Separator,       enabled: true });
                    }
                    if hit.is_md {
                        items.push(ContextMenuItem { label: "Open Preview",       shortcut: "Cmd+Shift+M", action: CtxAction::TabOpenPreview,  enabled: true });
                        items.push(ContextMenuItem { label: "",                   shortcut: "",            action: CtxAction::Separator,       enabled: true });
                    }
                    items.push(ContextMenuItem { label: "Split Right", shortcut: "", action: CtxAction::TabSplitRight, enabled: true });
                    items.push(ContextMenuItem { label: "Split Down",  shortcut: "", action: CtxAction::TabSplitDown,  enabled: true });
                    items.push(ContextMenuItem { label: "Split Left",  shortcut: "", action: CtxAction::TabSplitLeft,  enabled: true });
                    items.push(ContextMenuItem { label: "Split Up",    shortcut: "", action: CtxAction::TabSplitUp,    enabled: true });
                    items.push(ContextMenuItem { label: "",            shortcut: "", action: CtxAction::Separator,     enabled: true });
                    items.push(ContextMenuItem { label: "Close",       shortcut: "Cmd+W", action: CtxAction::TabClose, enabled: true });
                    s.context_menu = Some(ContextMenu {
                        x: mx, y: my, hovered: 0, tab_source: Some((hit.pid, hit.ti)), git_entry: None, explorer_entry: None, explorer_entry_is_dir: false, diag_message: None, items,
                    });
                } else {
                    // Editor text-area context menu
                    if let Some(pid) = pane_at_pos(&s.pane_tree, mx, my, area) {
                        if s.panes.get(&pid).map_or(false, |p| p.kind == PaneKind::Editor) {
                            s.active_pane = pid;
                            let lang = s.tab().lang();
                            let active_path = s.tab().path.clone();
                            let lsp_avail = lang != Lang::None
                                && active_path.as_ref().and_then(|p| {
                                    s.lsp.server_for_lang_host(lang, p.ssh_host()).map(|srv| srv.initialized)
                                }).unwrap_or(false);
                            let has_sel = s.tab().cursors.iter().any(|c| c.has_sel());

                            // Detect if right-click is over a diagnostic squiggle
                            let diag_message: Option<String> = {
                                let pane_rect = layout_tree(&s.pane_tree, area).into_iter()
                                    .find(|(id, _)| *id == pid)
                                    .map(|(_, r)| r);
                                pane_rect.and_then(|rect| {
                                    let lh = s.glyphs.lh;
                                    let cw = s.glyphs.cw;
                                    let tab_h = s.tab_h();
                                    let total_lines = s.panes[&pid].tab().text.len_lines();
                                    let gutter_w = State::gutter_w(total_lines, cw);
                                    let ed_x = rect.x + gutter_w;
                                    let scroll = s.panes[&pid].tab().scroll;
                                    let hscroll = s.panes[&pid].tab().hscroll;
                                    let line = scroll + ((my - rect.y - tab_h).max(0) / lh) as usize;
                                    let col = ((mx - ed_x).max(0) / cw) as usize + hscroll;
                                    active_path.as_ref()
                                        .and_then(|p| s.diagnostics.get(p))
                                        .and_then(|diags| diags.iter().find(|d| {
                                            d.line == line && col >= d.col_start && col < d.col_end.max(d.col_start + 1)
                                        }))
                                        .map(|d| d.message.clone())
                                })
                            };

                            let mut items = vec![
                                ContextMenuItem { label: "Go to Definition",    shortcut: "Cmd+Click / F12", action: CtxAction::GotoDefinition,  enabled: lsp_avail },
                                ContextMenuItem { label: "Find All References", shortcut: "Cmd+Shift+F12",   action: CtxAction::FindReferences,  enabled: lsp_avail },
                                ContextMenuItem { label: "Format Document",     shortcut: "Opt+Shift+F",     action: CtxAction::FormatDocument,  enabled: lsp_avail },
                                ContextMenuItem { label: "Organize Imports",    shortcut: "Opt+Shift+O",     action: CtxAction::OrganizeImports, enabled: lsp_avail },
                                ContextMenuItem { label: "",                    shortcut: "",                action: CtxAction::Separator,       enabled: true },
                                ContextMenuItem { label: "Copy",                shortcut: "Cmd+C",           action: CtxAction::Copy,            enabled: has_sel },
                                ContextMenuItem { label: "Cut",                 shortcut: "Cmd+X",           action: CtxAction::Cut,             enabled: has_sel },
                                ContextMenuItem { label: "Paste",               shortcut: "Cmd+V",           action: CtxAction::Paste,           enabled: true },
                            ];
                            if diag_message.is_some() {
                                items.push(ContextMenuItem { label: "", shortcut: "", action: CtxAction::Separator, enabled: true });
                                items.push(ContextMenuItem { label: "Copy Diagnostic", shortcut: "", action: CtxAction::CopyDiagnostic, enabled: true });
                            }
                            s.context_menu = Some(ContextMenu {
                                x: mx, y: my,
                                hovered: 0,
                                tab_source: None, git_entry: None, explorer_entry: None, explorer_entry_is_dir: false,
                                diag_message,
                                items,
                            });
                        }
                    }
                }
                { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
            }

            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left, ..
            } => {
                let mx  = s.mouse_x as i32;
                let my  = s.mouse_y as i32;
                let alt = s.mods.alt_key();
                let cmd = primary_mod(s.mods);
                let shift = s.mods.shift_key();

                // Quick finder overlay click (must run before other handlers)
                if s.quick_finder.open {
                    let cw = s.glyphs.cw;
                    let lh = s.glyphs.lh;
                    let ow = (s.w as i32 * 2 / 3).min(s.w as i32 - 40).max(360);
                    let ox = (s.w as i32 - ow) / 2;
                    let oy = s.h as i32 / 4;
                    // Same rows as the renderer draws: at most 10, scrolled to keep the
                    // selection in view (plus a placeholder row in some modes).
                    let qf = &s.quick_finder;
                    let is_cmd  = qf.query.starts_with('>');
                    let is_goto = qf.query.starts_with(':');
                    let n = if is_cmd { qf.filtered_commands.len() } else { qf.filtered.len() };
                    let view_start = qf.selected.saturating_sub(4).min(n.saturating_sub(10));
                    let visible = n.saturating_sub(view_start).min(10);
                    let direct_row = !is_cmd && !is_goto && visible == 0 && !qf.loading && qf.direct_path.is_some();
                    let item_count = if is_cmd { visible }
                        else if is_goto { 1 }
                        else { visible.max(direct_row as usize).max(qf.loading as usize) };
                    let oh = lh * (item_count as i32 + 2) + 8;
                    if !(mx >= ox && mx < ox + ow && my >= oy && my < oy + oh) {
                        // Click outside: dismiss (like Escape) instead of leaving the
                        // finder open and capturing keys while the click edits behind it.
                        quick_finder_cancel(s);
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                    let rows_y = oy + 4 + lh + 4;
                    if my >= rows_y && !is_goto {
                        // Click on a result row: select and open it (like Enter)
                        let row = ((my - rows_y) / lh) as usize;
                        let target = if direct_row { (row == 0).then_some(0) }
                            else if row < visible && !(qf.loading && !is_cmd) { Some(view_start + row) }
                            else { None };
                        if let Some(sel) = target {
                            s.quick_finder.selected = sel;
                            if quick_finder_accept(s) {
                                s.panes.clear();
                                el.exit();
                                return;
                            }
                        }
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                    {
                        // Click in input row
                        if my < oy + 4 + lh {
                            let is_cmd = s.quick_finder.query.starts_with('>');
                            let text_start_x = if is_cmd { ox + 4 + cw } else { ox + 4 + 2 * cw };
                            let field_clip = ox + ow - 4;
                            let vis = ((field_clip - text_start_x) / cw).max(0) as usize;
                            let cur_chars = s.quick_finder.query[..s.quick_finder.cursor].chars().count();
                            let new_byte = field_click_to_byte(&s.quick_finder.query, mx, text_start_x, cw, cur_chars, vis);
                            if shift && s.quick_finder.sel_anchor.is_none() {
                                s.quick_finder.sel_anchor = Some(s.quick_finder.cursor);
                            } else if !shift {
                                s.quick_finder.sel_anchor = None;
                            }
                            s.quick_finder.cursor = new_byte;
                        }
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                }

                // Dismiss context menu on any click outside it
                if let Some(ref cm) = s.context_menu {
                    let item_h = s.glyphs.lh + 2;
                    let sep_h  = 5i32;
                    let total_h: i32 = cm.items.iter().map(|i| if i.action == CtxAction::Separator { sep_h } else { item_h }).sum::<i32>() + 4;
                    let label_w = cm.items.iter().filter(|i| i.action != CtxAction::Separator).map(|i| i.label.chars().count()).max().unwrap_or(8) as i32;
                    let sc_w    = cm.items.iter().filter(|i| i.action != CtxAction::Separator).map(|i| key_label(i.shortcut).chars().count()).max().unwrap_or(0) as i32;
                    let menu_w  = (label_w + sc_w + 4) * s.glyphs.cw + 16;
                    let menu_x  = cm.x.min(s.w as i32 - menu_w).max(0);
                    let menu_y  = cm.y.min(s.h as i32 - total_h).max(0);
                    let in_menu = mx >= menu_x && mx < menu_x + menu_w && my >= menu_y && my < menu_y + total_h;
                    if in_menu {
                        let mut iy = menu_y + 2;
                        let mut action_taken: Option<CtxAction> = None;
                        for item in &cm.items {
                            let ih = if item.action == CtxAction::Separator { sep_h } else { item_h };
                            if my >= iy && my < iy + ih && item.action != CtxAction::Separator && item.enabled {
                                action_taken = Some(item.action);
                                break;
                            }
                            iy += ih;
                        }
                        let tab_source = cm.tab_source;
                        let git_entry  = cm.git_entry.clone();
                        let explorer_entry = cm.explorer_entry.clone();
                        let explorer_entry_is_dir = cm.explorer_entry_is_dir;
                        let diag_message = cm.diag_message.clone();
                        s.context_menu = None;
                        if let Some(action) = action_taken {
                            match action {
                                CtxAction::GitOpenFile => {
                                    if let Some((_, ref path)) = git_entry {
                                        if let Some(root) = git_root(s) {
                                            open_or_reuse_tab(s, root.join(path));
                                        }
                                    }
                                }
                                CtxAction::GitViewDiff => {
                                    if let Some((staged, ref path)) = git_entry {
                                        open_diff_tab(s, path.clone(), staged);
                                    }
                                }
                                CtxAction::GitStage => {
                                    if let Some((_, ref path)) = git_entry {
                                        run_git_op(s, vec!["add".into(), "--".into(), path.clone()]);
                                    }
                                }
                                CtxAction::GitUnstage => {
                                    if let Some((_, ref path)) = git_entry {
                                        run_git_op(s, vec!["restore".into(), "--staged".into(), "--".into(), path.clone()]);
                                    }
                                }
                                CtxAction::OpenSettings    => open_settings_tab(s),
                                CtxAction::GotoDefinition  => execute_command(s, CommandAction::GotoDefinition),
                                CtxAction::FindReferences  => execute_command(s, CommandAction::FindReferences),
                                CtxAction::FormatDocument  => execute_command(s, CommandAction::FormatDocument),
                                CtxAction::OrganizeImports => execute_command(s, CommandAction::OrganizeImports),
                                CtxAction::Copy  => { execute_command(s, CommandAction::Copy); }
                                CtxAction::Cut   => { execute_command(s, CommandAction::Cut); notify_collab_change(s); }
                                CtxAction::Paste => { execute_command(s, CommandAction::Paste); notify_collab_change(s); }
                                CtxAction::CopyDiagnostic => {
                                    if let Some(msg) = diag_message { clipboard_set(&msg); }
                                }
                                CtxAction::ExplorerNewFile => {
                                    if let Some(target) = explorer_entry {
                                        let parent = if explorer_entry_is_dir {
                                            target
                                        } else {
                                            target.parent().unwrap_or(target)
                                        };
                                        if let Some(name) = prompt_for_name("New file name", "") {
                                            let path = parent.join(name);
                                            match path.clone() {
                                                VPath::Local(local_path) => {
                                                    match std::fs::OpenOptions::new().write(true).create_new(true).open(&local_path) {
                                                        Ok(_) => {
                                                            refresh_explorer_now(s);
                                                            open_or_reuse_tab(s, path);
                                                        }
                                                        Err(err) => s.status_msg = Some(format!("Create file: {err}")),
                                                    }
                                                }
                                                VPath::Remote { host, path: remote_path } => {
                                                    ssh::ssh_create_file(
                                                        host,
                                                        remote_path,
                                                        parent.as_path().to_path_buf(),
                                                        s.proxy.clone(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                CtxAction::ExplorerNewFolder => {
                                    if let Some(target) = explorer_entry {
                                        let parent = if explorer_entry_is_dir {
                                            target
                                        } else {
                                            target.parent().unwrap_or(target)
                                        };
                                        if let Some(name) = prompt_for_name("New folder name", "") {
                                            let path = parent.join(name);
                                            match path {
                                                VPath::Local(path) => {
                                                    match std::fs::create_dir(path) {
                                                        Ok(_) => refresh_explorer_now(s),
                                                        Err(err) => s.status_msg = Some(format!("Create folder: {err}")),
                                                    }
                                                }
                                                VPath::Remote { host, path } => {
                                                    ssh::ssh_create_dir(
                                                        host,
                                                        path,
                                                        parent.as_path().to_path_buf(),
                                                        s.proxy.clone(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                CtxAction::ExplorerCopy => {
                                    s.explorer_clipboard = explorer_entry;
                                }
                                CtxAction::ExplorerDuplicate => {
                                    if let Some(source) = explorer_entry {
                                        if let Some(parent) = source.parent() {
                                            match (source, parent) {
                                                (VPath::Local(source), VPath::Local(parent)) => {
                                                    let destination = unique_copy_destination(&parent, &source);
                                                    match copy_path_recursively(&source, &destination) {
                                                        Ok(_) => refresh_explorer_now(s),
                                                        Err(err) => s.status_msg = Some(format!("Duplicate: {err}")),
                                                    }
                                                }
                                                (
                                                    VPath::Remote { host, path: source },
                                                    VPath::Remote { path: parent, .. },
                                                ) => {
                                                    ssh::ssh_copy_path(host, source, parent, s.proxy.clone());
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                CtxAction::ExplorerPaste => {
                                    if let (Some(source), Some(target)) = (
                                        s.explorer_clipboard.clone(),
                                        explorer_entry,
                                    ) {
                                        let parent = if explorer_entry_is_dir {
                                            target
                                        } else {
                                            target.parent().unwrap_or(target)
                                        };
                                        match (source, parent) {
                                            (VPath::Local(source), VPath::Local(parent)) => {
                                                let destination = paste_destination(&parent, &source);
                                                match copy_path_recursively(&source, &destination) {
                                                    Ok(_) => refresh_explorer_now(s),
                                                    Err(err) => s.status_msg = Some(format!("Paste: {err}")),
                                                }
                                            }
                                            (
                                                VPath::Remote { host: source_host, path: source },
                                                VPath::Remote { host: destination_host, path: parent },
                                            ) if source_host == destination_host => {
                                                ssh::ssh_copy_path(source_host, source, parent, s.proxy.clone());
                                            }
                                            _ => {
                                                s.status_msg = Some(
                                                    "Paste: source and destination must use the same filesystem".to_owned(),
                                                );
                                            }
                                        }
                                    }
                                }
                                CtxAction::ExplorerRename => {
                                    if let Some(old_path) = explorer_entry {
                                        let old_name = old_path.file_name().and_then(|name| name.to_str()).unwrap_or("");
                                        if let (Some(parent), Some(name)) = (
                                            old_path.parent(),
                                            prompt_for_name("Rename to", old_name),
                                        ) {
                                            let new_path = parent.join(name);
                                            match (&old_path, &new_path) {
                                                (VPath::Local(old), VPath::Local(new)) => {
                                                    if new.exists() {
                                                        s.status_msg = Some(format!("Rename: {} already exists", new.display()));
                                                    } else {
                                                        match std::fs::rename(old, new) {
                                                            Ok(_) => {
                                                                update_paths_after_rename(s, &old_path, &new_path);
                                                                refresh_explorer_now(s);
                                                            }
                                                            Err(err) => s.status_msg = Some(format!("Rename: {err}")),
                                                        }
                                                    }
                                                }
                                                (
                                                    VPath::Remote { host, path: old },
                                                    VPath::Remote { path: new, .. },
                                                ) => ssh::ssh_rename_path(
                                                    host.clone(),
                                                    old.clone(),
                                                    new.clone(),
                                                    parent.as_path().to_path_buf(),
                                                    s.proxy.clone(),
                                                ),
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                CtxAction::ExplorerOpenAsRoot => {
                                    if let Some(path) = explorer_entry {
                                        if let Some(uri) = remote_uri(&path) {
                                            open_remote_directory(s, path, uri);
                                        } else {
                                            s.explorer = Some(FileExplorer::new(path));
                                            s.left_view = LeftView::FileTree;
                                        }
                                    }
                                }
                                CtxAction::OpenTerminalHere => {
                                    if let Some(path) = explorer_entry {
                                        // For files, open a terminal in the parent directory;
                                        // for directories, use the directory itself.
                                        let dir = if !explorer_entry_is_dir {
                                            path.parent().unwrap_or(path)
                                        } else {
                                            path
                                        };
                                        open_terminal_pane_at(s, Some(dir));
                                    }
                                }
                                CtxAction::TabCopyRelPath => {
                                    if let Some((pid, ti)) = tab_source {
                                        let path = s.panes.get(&pid)
                                            .and_then(|p| p.tabs.get(ti))
                                            .and_then(|t| t.path.clone());
                                        if let Some(path) = path {
                                            let text = if let Some(local) = path.as_local_path() {
                                                let rel = std::env::current_dir().ok()
                                                    .and_then(|cwd| local.strip_prefix(&cwd).ok().map(|p| p.to_string_lossy().into_owned()))
                                                    .unwrap_or_else(|| local.to_string_lossy().into_owned());
                                                rel
                                            } else {
                                                path.display_short()
                                            };
                                            clipboard_set(&text);
                                        }
                                    }
                                }
                                CtxAction::TabCopyFullPath => {
                                    if let Some((pid, ti)) = tab_source {
                                        let path = s.panes.get(&pid)
                                            .and_then(|p| p.tabs.get(ti))
                                            .map(|t| t.path.as_ref().map(|p| p.display_short()).unwrap_or_default());
                                        if let Some(path) = path { clipboard_set(&path); }
                                    }
                                }
                                CtxAction::TabOpenPreview => {
                                    if let Some((pid, _)) = tab_source {
                                        s.active_pane = pid;
                                        open_markdown_preview(s);
                                    }
                                }
                                CtxAction::TabSplitRight => {
                                    if let Some((pid, _)) = tab_source { s.active_pane = pid; execute_command(s, CommandAction::SplitRight); }
                                }
                                CtxAction::TabSplitDown => {
                                    if let Some((pid, _)) = tab_source { s.active_pane = pid; execute_command(s, CommandAction::SplitDown); }
                                }
                                CtxAction::TabSplitLeft => {
                                    if let Some((pid, _)) = tab_source { s.active_pane = pid; execute_command(s, CommandAction::SplitLeft); }
                                }
                                CtxAction::TabSplitUp => {
                                    if let Some((pid, _)) = tab_source { s.active_pane = pid; execute_command(s, CommandAction::SplitUp); }
                                }
                                CtxAction::TabClose => {
                                    if let Some((pid, ti)) = tab_source {
                                        s.active_pane = pid;
                                        if request_close_editor_tab(s, pid, ti) == Some(true) {
                                            s.panes.clear();
                                            el.exit();
                                        }
                                    }
                                }
                                CtxAction::Separator => {}
                            }
                        }
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    } else {
                        s.context_menu = None;
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    }
                }

                // Activity bar click
                let act_w = s.activity_bar_w();
                if s.explorer.is_some() && mx < act_w && my < s.h as i32 - s.status_h() {
                    let lh = s.glyphs.lh;
                    let file_icon_y   = 8;
                    let srch_icon_y   = file_icon_y + lh + 4;
                    let diag_icon_y   = srch_icon_y + lh + 4;
                    let git_icon_y    = diag_icon_y + lh + 4;
                    let collab_icon_y = git_icon_y  + lh + 4;
                    let gear_y        = s.h as i32 - s.status_h() - lh - 8;
                    let clicked_view = if my >= file_icon_y && my < file_icon_y + lh {
                        Some(LeftView::FileTree)
                    } else if my >= srch_icon_y && my < srch_icon_y + lh {
                        Some(LeftView::GlobalSearch)
                    } else if my >= diag_icon_y && my < diag_icon_y + lh {
                        Some(LeftView::Diagnostics)
                    } else if my >= git_icon_y && my < git_icon_y + lh {
                        Some(LeftView::Git)
                    } else if s.collab.is_some() && my >= collab_icon_y && my < collab_icon_y + lh {
                        Some(LeftView::Collab)
                    } else {
                        None
                    };
                    if let Some(view) = clicked_view {
                        commit_settings_field(s);
                        blur_sidebar(s);
                        if s.left_panel_visible && s.left_view == view {
                            s.left_panel_visible = false;
                        } else {
                            s.left_view = view;
                            s.left_panel_visible = true;
                            match view {
                                LeftView::GlobalSearch => s.global_find.focus = GlobalFindFocus::Query,
                                // Arrow keys walk the problems list (Escape returns to the editor).
                                LeftView::Diagnostics  => s.sidebar_list_focused = true,
                                LeftView::Git => {
                                    s.git_panel.loading = true;
                                    trigger_git_status_refresh(s);
                                }
                                _ => {}
                            }
                        }
                    } else if my >= gear_y && my < gear_y + lh {
                        if s.context_menu.is_some() {
                            s.context_menu = None;
                        } else {
                            s.context_menu = Some(ContextMenu {
                                x: act_w, y: gear_y,
                                items: vec![ContextMenuItem { label: "Open Settings", shortcut: "", action: CtxAction::OpenSettings, enabled: true }],
                                hovered: 0,
                                tab_source: None, git_entry: None, explorer_entry: None, explorer_entry_is_dir: false, diag_message: None,
                            });
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Explorer border drag start (before pane border and explorer click)
                if s.explorer.is_some() {
                    let bx = act_w + s.explorer_w();
                    if (mx - bx).abs() <= BORDER_HIT && my < s.h as i32 - s.status_h() {
                        s.explorer_drag = true;
                        return;
                    }
                }

                // Pane border resize
                let area = s.pane_area();
                if let Some((path, axis, rect)) = find_border_at(&s.pane_tree, area, mx, my) {
                    s.resize_drag = Some(ResizeDrag { path, axis, rect });
                    return;
                }

                // Left panel click (file tree or global search)
                let explorer_w = s.explorer_w();
                if s.explorer.is_some() && mx >= act_w && mx < act_w + explorer_w {
                    commit_settings_field(s);
                    let lh  = s.glyphs.lh;
                    if s.left_view == LeftView::FileTree {
                        let cw  = s.glyphs.cw;
                        let row = my / lh;
                        if row == 0 {
                            // Hidden-files toggle row
                            if let Some(ex) = s.explorer.as_mut() { ex.toggle_hidden(); }
                        } else if row == 1 {
                            // Tree search row: fuzzy toggle or focus input
                            let fuzzy_toggle_w = 3 * cw + 6;
                            if mx - act_w < fuzzy_toggle_w {
                                if let Some(ex) = s.explorer.as_mut() {
                                    ex.tree_search_fuzzy = !ex.tree_search_fuzzy;
                                    refilter_tree_search(ex);
                                }
                            } else if let Some(ex) = s.explorer.as_mut() {
                                ex.tree_search_focused = true;
                                if ex.tree_search_entries.is_empty() {
                                    spawn_tree_search_walk(ex, &s.proxy);
                                }
                                // Position cursor at click point
                                let fuzzy_toggle_w = 3 * cw + 6;
                                let sx = act_w + fuzzy_toggle_w;
                                let pw = explorer_w;
                                let sw = (pw - fuzzy_toggle_w - 2).max(0);
                                let vis = ((sw - 4) / cw).max(0) as usize;
                                let cur_chars = ex.tree_search[..ex.tree_search_cursor].chars().count();
                                let new_byte = field_click_to_byte(&ex.tree_search, mx, sx + 2, cw, cur_chars, vis);
                                ex.tree_search_cursor = new_byte;
                                ex.tree_search_sel_anchor = None;
                            }
                        } else if row >= 2 {
                            let scroll_off = s.explorer.as_ref().map_or(0, |ex| ex.tree_scroll);
                            let idx = (row - 2) as usize + scroll_off;
                            let in_search = s.explorer.as_ref().map_or(false, |ex| !ex.tree_search.is_empty());
                            if in_search {
                                let search_idx = (row - 2) as usize; // search results not scrolled
                                let path = s.explorer.as_ref()
                                    .and_then(|ex| ex.tree_search_results.get(search_idx).cloned());
                                if let Some(path) = path { open_or_reuse_tab(s, VPath::Local(path)); }
                            } else {
                                // Unfocus tree search if clicking entries
                                if let Some(ex) = s.explorer.as_mut() { ex.tree_search_focused = false; }
                                let mut remote_dir_to_list: Option<(vpath::SshHost, PathBuf)> = None;
                                let action = s.explorer.as_mut().and_then(|ex| {
                                    if idx < ex.entries.len() {
                                        ex.selected = idx;
                                        if ex.entries[idx].is_dir {
                                            if let Some(path) = ex.toggle(idx) {
                                                if let VPath::Remote { host, .. } = &ex.root {
                                                    remote_dir_to_list = Some((host.clone(), path));
                                                }
                                            }
                                            None
                                        } else {
                                            Some(explorer_entry_vpath(ex, ex.entries[idx].path.clone()))
                                        }
                                    } else { None }
                                });
                                if let Some((host, path)) = remote_dir_to_list {
                                    if let Some(explorer) = s.explorer.as_mut() {
                                        explorer.pending_remote_listings.insert(path.clone());
                                    }
                                    ssh::ssh_list_dir(host, path, s.proxy.clone());
                                }
                                if let Some(path) = action { open_or_reuse_tab(s, path); }
                            }
                        }
                    } else if s.left_view == LeftView::Diagnostics {
                        // Diagnostics panel click — a row opens the file at that line
                        // (and the editor gets the keyboard); anywhere else in the panel
                        // focuses the list for Up/Down/Enter.
                        let header_rows = 2; // header label + separator
                        let first_item_y = header_rows * lh + 2 + 4;
                        s.sidebar_list_focused = true;
                        if my >= first_item_y {
                            let idx = ((my - first_item_y) / lh) as usize;
                            // Re-sort by severity (errors first) — same as diag_panel_snap
                            let mut sev_items: Vec<(usize, VPath, usize)> = s.diagnostics.iter()
                                .flat_map(|(path, diags)| {
                                    let sev_ord = |s: &DiagSeverity| match s { DiagSeverity::Error => 0, DiagSeverity::Warning => 1, _ => 2 };
                                    diags.iter().map(move |d| (sev_ord(&d.severity), path.clone(), d.line))
                                })
                                .collect();
                            sev_items.sort();
                            if idx < sev_items.len() {
                                s.diag_panel_sel = idx;
                                s.sidebar_list_focused = false;
                                let (_, path, line) = sev_items[idx].clone();
                                jump_to_path_line_col(s, path, line, 0);
                            }
                        }
                    } else if s.left_view == LeftView::Git {
                        // Git panel click — all y positions adjusted for panel scroll
                        let panel_h = s.h as i32 - s.status_h();
                        let scroll_px        = s.git_panel.scroll as i32 * lh;
                        let staged_header_y  = 4 - scroll_px;
                        let staged_start_y   = staged_header_y + lh;
                        let staged_count     = s.git_panel.staged.len();
                        let staged_rows      = staged_count.max(1);  // includes "(none)"
                        let changes_header_y = staged_start_y + staged_rows as i32 * lh + 2;
                        let unstaged_start_y = changes_header_y + lh;
                        let commit_area_top  = panel_h - lh * 3 - 8;
                        let commit_field_y   = commit_area_top + 4 + lh;
                        let commit_btn_y     = commit_field_y + lh + 2;
                        let cw_g = s.glyphs.cw;

                        if my >= staged_start_y && my < staged_start_y + staged_count as i32 * lh && staged_count > 0 {
                            let i = ((my - staged_start_y) / lh) as usize;
                            if i < staged_count {
                                s.git_panel.commit_focused = false;
                                s.git_panel.sel = GitSel::Staged(i);
                                let path = s.git_panel.staged[i].path.clone();
                                run_git_op(s, vec!["restore".into(), "--staged".into(), "--".into(), path]);
                            }
                        } else if my >= unstaged_start_y && my < commit_area_top {
                            let unstaged_count = s.git_panel.unstaged.len();
                            let i = ((my - unstaged_start_y) / lh) as usize;
                            if i < unstaged_count {
                                s.git_panel.commit_focused = false;
                                s.git_panel.sel = GitSel::Unstaged(i);
                                let path = s.git_panel.unstaged[i].path.clone();
                                open_diff_tab(s, path, false);
                            }
                        } else if my >= commit_field_y && my < commit_field_y + lh {
                            s.git_panel.commit_focused = true;
                            let commit_msg = s.git_panel.commit_msg.clone();
                            let field_x = act_w + 4;
                            let vis = ((explorer_w - 8) / cw_g).max(0) as usize;
                            let cur_chars = commit_msg[..s.git_panel.commit_cursor].chars().count();
                            let new_byte = field_click_to_byte(&commit_msg, mx, field_x, cw_g, cur_chars, vis);
                            s.git_panel.commit_cursor = new_byte;
                        } else if my >= commit_btn_y && my < commit_btn_y + lh {
                            let can_commit = !s.git_panel.staged.is_empty() && !s.git_panel.commit_msg.is_empty();
                            let has_unstaged = !s.git_panel.unstaged.is_empty();
                            if mx < act_w + explorer_w / 2 && can_commit {
                                let msg = s.git_panel.commit_msg.clone();
                                s.git_panel.commit_msg.clear();
                                s.git_panel.commit_cursor = 0;
                                s.git_panel.commit_focused = false;
                                run_git_op(s, vec!["commit".into(), "-m".into(), msg]);
                            } else if mx >= act_w + explorer_w / 2 && has_unstaged {
                                run_git_op(s, vec!["add".into(), "-A".into()]);
                            }
                        } else {
                            s.git_panel.commit_focused = false;
                        }
                    } else {
                        // Global search panel click — determine which field was clicked
                        let row_h = lh + 2;
                        let field_label_w = 9 * s.glyphs.cw;
                        let fields_start_y = 4;
                        let fields = [GlobalFindFocus::Query, GlobalFindFocus::Replace, GlobalFindFocus::Include, GlobalFindFocus::Exclude];
                        let search_btn_y = fields_start_y + 4 * row_h + 2;
                        let results_start_y = search_btn_y + row_h + 4 + lh;
                        if my >= fields_start_y && my < fields_start_y + 4 * row_h {
                            let fi = ((my - fields_start_y) / row_h).clamp(0, 3) as usize;
                            if mx >= act_w + field_label_w {
                                let gf_field_w = s.explorer_w() - field_label_w - 4;
                                let field_x    = act_w + field_label_w;
                                let cw_g       = s.glyphs.cw;
                                let vis = ((gf_field_w - 4) / cw_g).max(0) as usize;
                                s.global_find.focus = fields[fi];
                                s.sidebar_list_focused = false;
                                // Compute new cursor position from click (need field text + prev cursor)
                                let (field_text, prev_cursor) = match fields[fi] {
                                    GlobalFindFocus::Query   => (s.global_find.query.clone(),        s.global_find.cursor_query),
                                    GlobalFindFocus::Replace => (s.global_find.replace.clone(),      s.global_find.cursor_replace),
                                    GlobalFindFocus::Include => (s.global_find.include_glob.clone(), s.global_find.cursor_include),
                                    GlobalFindFocus::Exclude => (s.global_find.exclude_glob.clone(), s.global_find.cursor_exclude),
                                    _ => unreachable!(),
                                };
                                let cur_chars = field_text[..prev_cursor].chars().count();
                                let new_byte = field_click_to_byte(&field_text, mx, field_x + 2, cw_g, cur_chars, vis);
                                match fields[fi] {
                                    GlobalFindFocus::Query   => { s.global_find.cursor_query   = new_byte; s.global_find.sel_anchor_q   = None; }
                                    GlobalFindFocus::Replace => { s.global_find.cursor_replace = new_byte; s.global_find.sel_anchor_r   = None; }
                                    GlobalFindFocus::Include => { s.global_find.cursor_include = new_byte; s.global_find.sel_anchor_inc = None; }
                                    GlobalFindFocus::Exclude => { s.global_find.cursor_exclude = new_byte; s.global_find.sel_anchor_exc = None; }
                                    _ => {}
                                }
                            }
                        } else if my >= search_btn_y && my < search_btn_y + lh {
                            let field_label_w_px = field_label_w;
                            let field_x_abs = act_w + field_label_w_px;
                            if mx < field_x_abs {
                                // Live toggle (label area left of field_x)
                                s.global_find.live_search = !s.global_find.live_search;
                                // If switching to live and query is non-empty, fire immediately
                                if s.global_find.live_search && !s.global_find.query.is_empty() {
                                    s.global_find.search_fire_at =
                                        Some(Instant::now() + Duration::from_millis(300));
                                }
                            } else {
                            let search_label_w = "[Search]".chars().count() as i32 * s.glyphs.cw;
                            let ra_x = act_w + field_label_w + search_label_w + s.glyphs.cw;
                            if mx >= ra_x {
                                // Replace All button (must not start a new search first:
                                // that clears the results being replaced)
                                if !s.global_find.replace.is_empty() && !s.global_find.results.is_empty() {
                                    let results = s.global_find.results.clone();
                                    let query   = s.global_find.query.clone();
                                    let replace = s.global_find.replace.clone();
                                    let cs      = s.global_find.case_sensitive;
                                    global_replace(s, &results, &query, &replace, cs);
                                    s.global_find.results.clear();
                                }
                            } else {
                                // Search button
                                let root = s.explorer.as_ref()
                                    .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                                if !s.global_find.query.is_empty() {
                                    spawn_search(&mut s.global_find, root, s.proxy.clone());
                                    s.global_find.focus = GlobalFindFocus::Results;
                                    s.sidebar_list_focused = true;
                                }
                            }
                            } // end else (not live toggle)
                        } else if my >= results_start_y {
                            // A result row opens it (the editor gets the keyboard); empty
                            // space below the results focuses the list.
                            let ri = ((my - results_start_y) / lh) as usize + s.global_find.scroll;
                            s.global_find.focus = GlobalFindFocus::Results;
                            if ri < s.global_find.results.len() {
                                s.global_find.selected = ri;
                                s.sidebar_list_focused = false;
                                let r = s.global_find.results[ri].clone();
                                jump_to_path_line_col(s, r.path, r.line_num, r.match_col);
                            } else {
                                s.sidebar_list_focused = true;
                            }
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Status bar — ignore
                if my >= s.h as i32 - s.status_h() { return; }

                // Which pane was clicked?
                let area = s.pane_area();
                let Some(clicked_pane_id) = pane_at_pos(&s.pane_tree, mx, my, area) else { return };
                if let Some(pane) = s.panes.get_mut(&clicked_pane_id) {
                    pane.find.input_focused = false;
                }
                // Clicking any content pane takes the keyboard from the sidebar (search
                // fields, commit message, lists) and finishes a settings-field edit.
                blur_sidebar(s);
                commit_settings_field(s);
                let pane_rect = layout_tree(&s.pane_tree, area).into_iter()
                    .find(|(id, _)| *id == clicked_pane_id).map(|(_, r)| r).unwrap_or(area);

                let pane_local_y = my - pane_rect.y;
                let tab_h        = s.tab_h();

                if pane_local_y < tab_h {
                    let cw = s.glyphs.cw;

                    // Terminal pane tab bar
                    if s.panes[&clicked_pane_id].kind == PaneKind::Terminal {
                        let mut tx = pane_rect.x;
                        let n_terms = s.panes[&clicked_pane_id].term_ids.len();
                        let mut hit = false;
                        for i in 0..n_terms {
                            let title_len = {
                                let tid = s.panes[&clicked_pane_id].term_ids[i];
                                s.term_panes.get(&tid).map(|tp| tp.title.chars().count()).unwrap_or(8)
                            };
                            let tw = (title_len + 3) as i32 * cw;
                            if mx < tx + tw {
                                hit = true;
                                if mx >= tx + tw - cw {
                                    // × close: LSP output panes hide (minimize); regular terminals close
                                    s.active_pane = clicked_pane_id;
                                    if s.lsp.servers.contains_key(&clicked_pane_id) {
                                        hide_lsp_output_pane(s, clicked_pane_id);
                                    } else if close_terminal_tab(s, clicked_pane_id, i) {
                                        s.panes.clear();
                                        el.exit();
                                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                                        return;
                                    }
                                    check_lsp_binaries_if_settings_open(s);
                                } else {
                                    s.panes.get_mut(&clicked_pane_id).unwrap().active = i;
                                    s.active_pane = clicked_pane_id;
                                    s.drag_pending = Some((clicked_pane_id, i, s.mouse_x, s.mouse_y));
                                }
                                break;
                            }
                            tx += tw + 1;
                        }
                        if !hit { s.active_pane = clicked_pane_id; }
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }

                    // Editor pane tab bar
                    let mut tx = pane_rect.x;
                    let n_tabs = s.panes[&clicked_pane_id].tabs.len();
                    for i in 0..n_tabs {
                        let (name_len, dirty) = {
                            let t = &s.panes[&clicked_pane_id].tabs[i];
                            (t.display_name().chars().count(), t.dirty)
                        };
                        let label_chars = name_len + if dirty { 4 } else { 3 };
                        let tw = label_chars as i32 * cw + 1;
                        if mx < tx + tw {
                            // × button: last cw pixels of the tab
                            if mx >= tx + tw - cw {
                                s.active_pane = clicked_pane_id;
                                let pane_id = clicked_pane_id;
                                if request_close_editor_tab(s, pane_id, i) == Some(true) {
                                    s.panes.clear();
                                    el.exit();
                                }
                                { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                                return;
                            }
                            s.panes.get_mut(&clicked_pane_id).unwrap().active = i;
                            s.active_pane = clicked_pane_id;
                            s.drag_pending = Some((clicked_pane_id, i, s.mouse_x, s.mouse_y));
                            { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                            return;
                        }
                        tx += tw;
                    }
                    s.active_pane = clicked_pane_id;
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Switch active pane on click
                s.active_pane = clicked_pane_id;

                // Non-editor panes (Terminal) have no tabs/find bar/cursors
                if s.panes[&clicked_pane_id].kind == PaneKind::Terminal {
                    let content_y = pane_rect.y + tab_h;
                    if my >= content_y {
                        let cw = s.glyphs.cw;
                        let lh = s.glyphs.lh;
                        let tid = s.panes[&clicked_pane_id].term_ids
                            .get(s.panes[&clicked_pane_id].active).copied();
                        if let Some(tid) = tid {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                let term_col = ((mx - pane_rect.x) / cw)
                                    .clamp(0, tp.grid.cols as i32 - 1) as usize;
                                let term_row = ((my - content_y) / lh)
                                    .clamp(0, tp.grid.rows as i32 - 1) as usize;
                                let mouse_capture = tp.grid.mouse_report != terminal::MouseReportMode::None;
                                if mouse_capture && !s.mods.shift_key() {
                                    // Forward to PTY
                                    let mut mod_bits: u8 = 0;
                                    if s.mods.shift_key()   { mod_bits |= 4; }
                                    if s.mods.alt_key()     { mod_bits |= 8; }
                                    if s.mods.control_key() { mod_bits |= 16; }
                                    let sgr = tp.grid.mouse_sgr;
                                    let pty = tp.input();
                                    let bytes = terminal::encode_mouse(term_col, term_row, mod_bits, true, sgr);
                                    pty.write(&bytes);
                                    s.term_buttons_held |= 1;
                                } else {
                                    // Track click count for double-click detection
                                    let now  = Instant::now();
                                    let fast = now.duration_since(s.term_last_click_time) < Duration::from_millis(500);
                                    let same = term_row == s.term_last_click_row && term_col == s.term_last_click_col;
                                    s.term_click_count = if fast && same { s.term_click_count + 1 } else { 1 };
                                    s.term_last_click_time = now;
                                    s.term_last_click_row  = term_row;
                                    s.term_last_click_col  = term_col;

                                    // Collect rows before releasing the borrow on tp
                                    let rows = tp.grid.visible_rows();

                                    if cmd {
                                        // Cmd+Click: open token under cursor
                                        if term_row < rows.len() {
                                            let (lo, hi) = term_token_bounds(&rows[term_row], term_col);
                                            let token: String = rows[term_row][lo..=hi].iter().map(|c| c.ch).collect();
                                            let child_pid = tp.child_pid();
                                            let osc_cwd   = tp.grid.cwd.clone();
                                            drop(rows);
                                            open_token(s, &token, child_pid, osc_cwd);
                                        }
                                    } else if s.term_click_count == 2 {
                                        // Double-click: select word on this row
                                        if term_row < rows.len() {
                                            let (lo, hi) = match s.settings.term_word_select {
                                                settings::TermWordSelect::Word =>
                                                    term_word_bounds(&rows[term_row], term_col),
                                                settings::TermWordSelect::Whitespace =>
                                                    term_token_bounds(&rows[term_row], term_col),
                                            };
                                            s.term_sel = Some(TermSel {
                                                start_vi: term_row, start_col: lo,
                                                end_vi:   term_row, end_col:   hi,
                                            });
                                            s.term_selecting = false;
                                        }
                                    } else {
                                        // Single click: start drag selection
                                        s.term_sel = Some(TermSel {
                                            start_vi: term_row, start_col: term_col,
                                            end_vi:   term_row, end_col:   term_col,
                                        });
                                        s.term_selecting = true;
                                    }
                                }
                            }
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }
                if s.panes[&clicked_pane_id].kind != PaneKind::Editor {
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Settings tab: intercept content-area clicks for button handling
                if s.panes[&clicked_pane_id].tabs.get(s.panes[&clicked_pane_id].active)
                        .map_or(false, |t| t.kind == TabKind::Settings) {
                    let tab_h     = s.tab_h();
                    let cw        = s.glyphs.cw;
                    let lh        = s.glyphs.lh;
                    let content_y = pane_rect.y + tab_h;
                    let scroll_px = s.panes[&clicked_pane_id].tabs.get(s.panes[&clicked_pane_id].active)
                        .map_or(0, |t| t.scroll as i32 * lh);
                    let btn_x     = pane_rect.x + 14 * cw;
                    let ry        = content_y + lh + 8 - scroll_px;
                    let cpu_db_y  = ry + lh + 4;
                    let gpu_dc_y  = cpu_db_y + lh + 4;
                    let vy        = gpu_dc_y + lh + 4;
                    let sy        = vy + lh + 4;
                    let rb_y      = sy + lh + 4;
                    let gc_y      = rb_y + lh + 4;
                    let ul_y      = gc_y + lh + 4;
                    let ul_num_y  = ul_y + lh + 4;
                    if my >= ry && my < ry + lh {
                        if mx >= btn_x && mx < btn_x + 5 * cw && s.renderer.is_gpu() {
                            s.settings.renderer = settings::RendererBackend::Cpu;
                            s.renderer = platform::Renderer::new_cpu(&s.win, s.settings.cpu_double_buffer);
                            s.renderer.resize(s.w, s.h);
                            s.settings.save();
                        } else if mx >= btn_x + 6 * cw && mx < btn_x + 11 * cw && !s.renderer.is_gpu() {
                            s.settings.renderer = settings::RendererBackend::Gpu;
                            s.renderer = platform::Renderer::new_gpu(&s.win, s.settings.gpu_drawable_count);
                            s.renderer.resize(s.w, s.h);
                            s.settings.save();
                        }
                    } else if my >= cpu_db_y && my < cpu_db_y + lh && mx >= btn_x && mx < btn_x + 8 * cw {
                        s.settings.cpu_double_buffer = !s.settings.cpu_double_buffer;
                        s.renderer.set_cpu_double_buffer(s.settings.cpu_double_buffer);
                        s.settings.save();
                    } else if my >= gpu_dc_y && my < gpu_dc_y + lh {
                        let new_count: u8 = if mx >= btn_x && mx < btn_x + 3 * cw { 2 }
                                            else if mx >= btn_x + 4 * cw && mx < btn_x + 7 * cw { 3 }
                                            else { s.settings.gpu_drawable_count };
                        if new_count != s.settings.gpu_drawable_count {
                            s.settings.gpu_drawable_count = new_count;
                            s.renderer.set_gpu_drawable_count(new_count);
                            s.settings.save();
                        }
                    } else if my >= vy && my < vy + lh && mx >= btn_x && mx < btn_x + 8 * cw {
                        s.settings.vsync = !s.settings.vsync;
                        s.settings.save();
                        let _ = s;
                        self.apply_vsync_setting();
                        let s = self.state.as_mut().unwrap();
                        s.needs_redraw = true;
                        self.dirty.store(true, Ordering::Release);
                        return;
                    } else if my >= rb_y && my < rb_y + lh && mx >= btn_x && mx < btn_x + 8 * cw {
                        s.settings.rainbow_brackets = !s.settings.rainbow_brackets;
                        s.settings.save();
                        for pane in s.panes.values_mut() {
                            for tab in pane.tabs.iter_mut() { tab.hl_color_cache.clear(); tab.hl_dirty_from = 0; }
                        }
                    } else if my >= gc_y && my < gc_y + lh {
                        // Glyph cache limit buttons — offsets must match gc_btns in render
                        let gc_offsets: [(i32, i32, settings::GlyphCacheLimit); 5] = [
                            (0, 11, settings::GlyphCacheLimit::Unlimited),
                            (12, 5, settings::GlyphCacheLimit::N512),
                            (18, 6, settings::GlyphCacheLimit::N1024),
                            (25, 6, settings::GlyphCacheLimit::N2048),
                            (32, 6, settings::GlyphCacheLimit::N4096),
                        ];
                        for (off, wid, variant) in gc_offsets {
                            if mx >= btn_x + off * cw && mx < btn_x + (off + wid) * cw {
                                s.settings.glyph_cache_limit = variant;
                                s.glyphs.max_entries = variant.cap();
                                s.settings.save();
                                break;
                            }
                        }
                    } else if my >= ul_y && my < ul_y + lh && mx >= btn_x && mx < btn_x + 12 * cw {
                        s.settings.undo_limit = if s.settings.undo_limit.is_none() { settings::Settings::default_undo_limit() } else { None };
                        s.settings.save();
                    } else if s.settings.undo_limit.is_some() && my >= ul_num_y && my < ul_num_y + lh {
                        let lim = s.settings.undo_limit.unwrap_or(200);
                        if mx >= btn_x && mx < btn_x + 3 * cw {
                            s.settings.undo_limit = Some((lim.saturating_sub(50)).max(50));
                            s.settings.save();
                        } else if mx >= btn_x + 4 * cw {
                            s.settings.undo_limit = Some((lim + 50).min(10_000));
                            s.settings.save();
                        }
                    } else {
                        // Terminal section — y-positions mirror the render function
                        let info_y    = if s.settings.undo_limit.is_some() { ul_y + lh * 2 + 8 } else { ul_y + lh + 4 };
                        let term_sec_y = info_y + lh + 8;
                        let tcp_y     = term_sec_y + lh + 4;
                        let tcb_y     = tcp_y + lh + 4;
                        let tab_y     = tcb_y + lh + 4;
                        let tws_y     = tab_y + lh + 4;
                        if my >= tcp_y && my < tcp_y + lh && mx >= btn_x && mx < btn_x + 8 * cw {
                            s.settings.term_copy_paste = !s.settings.term_copy_paste;
                            s.settings.save();
                        } else if my >= tcb_y && my < tcb_y + lh && mx >= btn_x && mx < btn_x + 8 * cw {
                            s.settings.term_cmd_bs = !s.settings.term_cmd_bs;
                            s.settings.save();
                        } else if my >= tab_y && my < tab_y + lh && mx >= btn_x && mx < btn_x + 8 * cw {
                            s.settings.term_alt_bs = !s.settings.term_alt_bs;
                            s.settings.save();
                        } else if my >= tws_y && my < tws_y + lh {
                            if mx >= btn_x && mx < btn_x + 11 * cw {
                                s.settings.term_word_select = settings::TermWordSelect::Whitespace;
                                s.settings.save();
                            } else if mx >= btn_x + 12 * cw && mx < btn_x + 18 * cw {
                                s.settings.term_word_select = settings::TermWordSelect::Word;
                                s.settings.save();
                            }
                        } else {
                            // Language Servers section — install button clicks
                            let lsp_sec_y = tws_y + lh + 8;
                            let lsp_rows: [Lang; 3] = [Lang::TypeScript, Lang::Rust, Lang::Python];
                            let scope_host = lsp_scope_host(s);
                            let inst_btn_x = btn_x + 14 * cw;
                            let inst_btn_w = 9 * cw;
                            for (i, &lang) in lsp_rows.iter().enumerate() {
                                let ly = lsp_sec_y + lh + 4 + i as i32 * (lh + 4);
                                let show_install = lsp_installed_for(s, lang, scope_host.as_ref()) == Some(false);
                                if show_install && my >= ly && my < ly + lh && mx >= inst_btn_x && mx < inst_btn_x + inst_btn_w {
                                    let Some(cmd) = lsp_install_command(lang) else { continue };
                                    open_terminal_pane(s);
                                    let pane_id = s.active_pane;
                                    if let Some(&tid) = s.panes.get(&pane_id).and_then(|p| p.term_ids.get(p.active)) {
                                        if let Some(tp) = s.term_panes.get(&tid) {
                                            let bytes = cmd.as_bytes();
                                            tp.write_input(bytes);
                                        }
                                    }
                                }
                            }
                            // Save Actions section — text field activation
                            let save_sec_y = lsp_sec_y + lh + 4 + 3 * (lh + 4) + 4;
                            let save_btn_x = pane_rect.x + 16 * cw;
                            let field_w = (pane_rect.w - 16 * cw - 4).max(cw);
                            let sa_field_ids = [SettingsFieldId::FormatOnSave, SettingsFieldId::OrganizeImportsOnSave, SettingsFieldId::FormatCommand];
                            let mut clicked_field = false;
                            for (i, fid) in sa_field_ids.iter().enumerate() {
                                let fy = save_sec_y + lh + 4 + i as i32 * (lh + 4);
                                if my >= fy && my < fy + lh && mx >= save_btn_x && mx < save_btn_x + field_w {
                                    let current = match fid {
                                        SettingsFieldId::FormatOnSave          => s.settings.format_on_save.clone(),
                                        SettingsFieldId::OrganizeImportsOnSave => s.settings.organize_imports_on_save.clone(),
                                        SettingsFieldId::FormatCommand         => s.settings.format_command.clone(),
                                        _ => unreachable!(),
                                    };
                                    let cursor = current.chars().count();
                                    s.settings_edit_field  = Some(*fid);
                                    s.settings_edit_text   = current;
                                    s.settings_edit_cursor = cursor;
                                    clicked_field = true;
                                    break;
                                }
                            }

                            // Collaboration section — text fields + toggles
                            let collab_sec_y = save_sec_y + lh + 4 + 3 * (lh + 4) + 8;
                            let perm_col_w = 4 * cw;
                            if !clicked_field {
                                // Port field
                                let cp_y = collab_sec_y + lh + 4;
                                if my >= cp_y && my < cp_y + lh && mx >= save_btn_x && mx < save_btn_x + 6 * cw {
                                    let current = s.settings.collab_port.to_string();
                                    let cursor = current.chars().count();
                                    s.settings_edit_field  = Some(SettingsFieldId::CollabPort);
                                    s.settings_edit_text   = current;
                                    s.settings_edit_cursor = cursor;
                                    clicked_field = true;
                                }
                                // Default role buttons
                                let dr_y = collab_sec_y + 2 * (lh + 4);
                                if !clicked_field && my >= dr_y && my < dr_y + lh {
                                    let role_btns: &[(collab::PeerRole, &str)] = &[
                                        (collab::PeerRole::Viewer,       " Viewer "),
                                        (collab::PeerRole::Contributor,  " Contributor "),
                                        (collab::PeerRole::SystemAccess, " Sys.Access "),
                                        (collab::PeerRole::Moderator,    " Moderator "),
                                    ];
                                    let mut rx2 = save_btn_x;
                                    for (role, label) in role_btns.iter() {
                                        let rw2 = label.chars().count() as i32 * cw;
                                        if mx >= rx2 && mx < rx2 + rw2 {
                                            s.settings.collab_role_perms.default_role = *role;
                                            s.settings.save();
                                            sync_collab_settings(s);
                                            clicked_field = true;
                                            break;
                                        }
                                        rx2 += rw2 + cw;
                                    }
                                }
                                // Include globs field
                                let cig_y = collab_sec_y + 3 * (lh + 4);
                                if !clicked_field && my >= cig_y && my < cig_y + lh && mx >= save_btn_x && mx < save_btn_x + field_w {
                                    let current = s.settings.collab_include_globs.join(",");
                                    let cursor = current.chars().count();
                                    s.settings_edit_field  = Some(SettingsFieldId::CollabIncludeGlobs);
                                    s.settings_edit_text   = current;
                                    s.settings_edit_cursor = cursor;
                                    clicked_field = true;
                                }
                                // Exclude globs field
                                let ceg_y = collab_sec_y + 4 * (lh + 4);
                                if !clicked_field && my >= ceg_y && my < ceg_y + lh && mx >= save_btn_x && mx < save_btn_x + field_w {
                                    let current = s.settings.collab_exclude_globs.join(",");
                                    let cursor = current.chars().count();
                                    s.settings_edit_field  = Some(SettingsFieldId::CollabExcludeGlobs);
                                    s.settings_edit_text   = current;
                                    s.settings_edit_cursor = cursor;
                                    clicked_field = true;
                                }
                                // Permission matrix toggles
                                let perm_hdr_y = collab_sec_y + 5 * (lh + 4) + 4;
                                let perm_bits = [
                                    collab::perms::VIEW_FILES, collab::perms::WRITE_FILES,
                                    collab::perms::VIEW_HIDDEN, collab::perms::WRITE_HIDDEN,
                                    collab::perms::VIEW_TERMINALS, collab::perms::OPEN_TERMINALS,
                                    collab::perms::MANAGE_ROLES,
                                ];
                                for (ri, _role) in [
                                    collab::PeerRole::Viewer,
                                    collab::PeerRole::Contributor,
                                    collab::PeerRole::SystemAccess,
                                    collab::PeerRole::Moderator,
                                ].iter().enumerate() {
                                    if clicked_field { break; }
                                    let pm_y = perm_hdr_y + lh + 4 + ri as i32 * (lh + 2);
                                    if my < pm_y || my >= pm_y + lh { continue; }
                                    for (ci, &bit) in perm_bits.iter().enumerate() {
                                        let px2 = save_btn_x + ci as i32 * perm_col_w;
                                        if mx >= px2 && mx < px2 + 3 * cw {
                                            let field = match ri {
                                                0 => &mut s.settings.collab_role_perms.viewer,
                                                1 => &mut s.settings.collab_role_perms.contributor,
                                                2 => &mut s.settings.collab_role_perms.system_access,
                                                _ => &mut s.settings.collab_role_perms.moderator,
                                            };
                                            *field ^= bit;
                                            s.settings.save();
                                            // Apply to the running session (enforcement uses the
                                            // session's copy) and tell guests.
                                            sync_collab_settings(s);
                                            clicked_field = true;
                                            break;
                                        }
                                    }
                                }
                            }

                            if !clicked_field {
                                // Click outside the fields (normally already committed when the
                                // pane click took focus).
                                commit_settings_field(s);
                            }
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Find bar click
                let fh = { let ap = clicked_pane_id; State::pane_find_h(&s.panes[&ap], s.glyphs.lh) };
                let in_find = fh > 0 && {
                    let fy = pane_rect.y + pane_rect.h - fh;
                    my >= fy && my < fy + fh
                };
                if in_find {
                    s.find_mut().input_focused = true;
                    let find_y  = pane_rect.y + pane_rect.h - fh;
                    let row_h   = s.glyphs.lh + 4;
                    let cw      = s.glyphs.cw;
                    let rel_row = (my - find_y) / row_h;
                    if rel_row == 0 {
                        let aa_len   = 4usize;
                        let w_len    = 3usize;
                        let toggle_w = (aa_len + 1 + w_len) as i32 * cw + 8;
                        // Drawn at the pane's right edge, not the window's (split panes).
                        let aa_x     = pane_rect.x + pane_rect.w - toggle_w;
                        let wl_x     = aa_x + (aa_len + 1) as i32 * cw;
                        if mx >= aa_x && mx < wl_x { s.find_mut().case_sensitive = !s.find().case_sensitive; }
                        else if mx >= wl_x         { s.find_mut().whole_word     = !s.find().whole_word; }
                        else {
                            // Click in query field — focus and position cursor
                            let lw = "Find: ".len() as i32 * cw;
                            let qx = pane_rect.x + 4 + lw;
                            let qclip = aa_x - cw;
                            let q_vis = ((qclip - qx) / cw).max(0) as usize;
                            let f = s.find_mut();
                            f.input_focused = true;
                            f.focus = FindFocus::Query;
                            f.cursor_query = clamp_byte_index(&f.query, f.cursor_query);
                            let cur_chars = f.query[..f.cursor_query].chars().count();
                            let new_byte = field_click_to_byte(&f.query, mx, qx, cw, cur_chars, q_vis);
                            f.cursor_query = new_byte;
                            f.sel_anchor_q = None;
                        }
                    } else if rel_row == 1 && s.find().replace_open {
                        let repl_len = 6usize;
                        let all_len  = 5usize;
                        let btn_w    = (repl_len + 1 + all_len) as i32 * cw + 8;
                        let btn_x    = pane_rect.x + pane_rect.w - btn_w;
                        let all_x    = btn_x + (repl_len + 1) as i32 * cw;
                        if mx >= btn_x && mx < all_x { replace_current(s); }
                        else if mx >= all_x          { replace_all(s); }
                        else {
                            // Click in replace field — focus and position cursor
                            let rlw = "Replace: ".len() as i32 * cw;
                            let rx = pane_rect.x + 4 + rlw;
                            let rclip = btn_x - cw;
                            let r_vis = ((rclip - rx) / cw).max(0) as usize;
                            let f = s.find_mut();
                            f.input_focused = true;
                            f.focus = FindFocus::Replace;
                            f.cursor_replace = clamp_byte_index(&f.replace, f.cursor_replace);
                            let cur_chars = f.replace[..f.cursor_replace].chars().count();
                            let new_byte = field_click_to_byte(&f.replace, mx, rx, cw, cur_chars, r_vis);
                            f.cursor_replace = new_byte;
                            f.sel_anchor_r = None;
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Editor area click
                let pos = s.xy_to_char(mx, my);
                let now  = Instant::now();
                let fast = now.duration_since(s.last_click_time) < Duration::from_millis(500);
                let same = pos == s.last_click_char;
                s.click_count = if fast && same { s.click_count + 1 } else { 1 };
                s.last_click_time = now;
                s.last_click_char = pos;
                if cmd {
                    // Cmd+Click: try LSP goto-definition first, fallback to path token open
                    let click_line = s.tab().text.char_to_line(pos.min(s.tab().text.len_chars().saturating_sub(1)));
                    let line_start = s.tab().text.line_to_char(click_line);
                    let click_col  = pos.saturating_sub(line_start);
                    let tab_lang   = s.tab().lang();
                    let tab_path   = s.tab().path.clone();
                    let mut lsp_sent = false;
                    if tab_lang != Lang::None {
                        if let Some(ref path) = tab_path {
                            if let Some(srv) = s.lsp.server_for_lang_host_mut(tab_lang, path.ssh_host()) {
                                if srv.initialized {
                                    lsp::request_definition(srv, path, click_line, click_col);
                                    lsp_sent = true;
                                }
                            }
                        }
                    }
                    if !lsp_sent {
                        let len = s.tab().text.len_chars();
                        if pos < len && !s.tab().text.char(pos).is_whitespace() {
                            let mut lo = pos;
                            while lo > 0 && !s.tab().text.char(lo - 1).is_whitespace() { lo -= 1; }
                            let mut hi = pos + 1;
                            while hi < len && !s.tab().text.char(hi).is_whitespace() { hi += 1; }
                            let token: String = s.tab().text.slice(lo..hi).chars().collect();
                            open_token(s, &token, None, None);
                        }
                    }
                } else if alt {
                    if s.click_count == 2 {
                        let (lo, hi) = word_bounds_at(s.tab(), pos);
                        if lo < hi {
                            s.tab_mut().cursors.retain(|cursor| {
                                cursor.hi() <= lo || cursor.lo() >= hi
                            });
                            s.tab_mut().cursors.push(Cursor { head: hi, tail: lo });
                        }
                        s.mouse_down = false;
                    } else {
                        s.tab_mut().cursors.retain(|cursor| {
                            cursor.has_sel() || cursor.head != pos
                        });
                        s.tab_mut().cursors.push(Cursor::new(pos));
                        s.mouse_down = shift;
                    }
                } else if shift {
                    let anchor = s.tab().primary().tail;
                    s.tab_mut().cursors = vec![Cursor { head: pos, tail: anchor }];
                    s.mouse_down = true;
                } else {
                    match s.click_count {
                        2 => {
                            let (lo, hi) = word_bounds_at(s.tab(), pos);
                            s.tab_mut().cursors = vec![Cursor { head: hi, tail: lo }];
                            s.mouse_down = false;
                        }
                        n if n >= 3 => {
                            let len = s.tab().text.len_chars();
                            let li  = s.tab().text.char_to_line(pos.min(len));
                            let ls  = s.tab().text.line_to_char(li);
                            let le  = if li + 1 < s.tab().text.len_lines() {
                                s.tab().text.line_to_char(li + 1)
                            } else { len };
                            s.tab_mut().cursors = vec![Cursor { head: le, tail: ls }];
                            s.mouse_down = false;
                        }
                        _ => {
                            s.tab_mut().cursors = vec![Cursor { head: pos, tail: pos }];
                            s.mouse_down = true;
                        }
                    }
                }
                s.reset_blink();
                { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x as i32, -y as i32),
                    MouseScrollDelta::PixelDelta(p)   => {
                        let lh = s.glyphs.lh as f64;
                        let cw = s.glyphs.cw as f64;
                        s.scroll_frac_y -= p.y;
                        let dy_int = (s.scroll_frac_y / lh).trunc() as i32;
                        s.scroll_frac_y -= dy_int as f64 * lh;
                        let dx_int = -(p.x / cw).trunc() as i32;
                        (dx_int, dy_int)
                    }
                };
                // Scroll global find results when hovering over the left panel
                let act_w = s.activity_bar_w();
                let mx = s.mouse_x as i32;
                if s.explorer.is_some() && s.left_view == LeftView::GlobalSearch
                    && mx >= act_w && mx < act_w + s.explorer_w() && dy != 0
                {
                    let n = s.global_find.results.len();
                    if dy > 0 {
                        s.global_find.scroll = (s.global_find.scroll + dy as usize).min(n.saturating_sub(1));
                    } else {
                        s.global_find.scroll = s.global_find.scroll.saturating_sub((-dy) as usize);
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }
                // Intercept scroll when mouse is over the left panel — don't pass to editor
                if s.explorer.is_some() && s.left_panel_visible
                    && mx >= act_w && mx < act_w + s.explorer_w() && dy != 0
                {
                    if s.left_view == LeftView::Git {
                        let total = s.git_panel.staged.len() + s.git_panel.unstaged.len();
                        s.git_panel.scroll = (s.git_panel.scroll as i32 + dy)
                            .max(0) as usize;
                        s.git_panel.scroll = s.git_panel.scroll.min(total.saturating_sub(1));
                    } else if s.left_view == LeftView::FileTree {
                        if let Some(ref mut ex) = s.explorer {
                            let n = ex.entries.len();
                            if dy > 0 {
                                ex.tree_scroll = (ex.tree_scroll + dy as usize).min(n.saturating_sub(1));
                            } else {
                                ex.tree_scroll = ex.tree_scroll.saturating_sub((-dy) as usize);
                            }
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }
                let pane_kind = s.panes.get(&s.active_pane).map(|p| p.kind.clone()).unwrap_or(PaneKind::Editor);
                match pane_kind {
                    PaneKind::Terminal => {
                        if dy != 0 {
                            let tid = s.panes.get(&s.active_pane)
                                .and_then(|p| p.term_ids.get(p.active).copied());
                            let Some(tid) = tid else { return; };
                            // Extract what we need before taking the mutable borrow
                            let mouse_report = s.term_panes.get(&tid)
                                .map(|tp| tp.grid.mouse_report).unwrap_or(terminal::MouseReportMode::None);
                            let scroll_offset = s.term_panes.get(&tid)
                                .map(|tp| tp.grid.scroll_offset).unwrap_or(0);
                            if mouse_report != terminal::MouseReportMode::None && scroll_offset == 0 {
                                // Forward scroll to PTY as mouse button events
                                let area = s.pane_area();
                                let pane_id = s.active_pane;
                                let pane_rect = layout_tree(&s.pane_tree, area).into_iter()
                                    .find(|(id, _)| *id == pane_id).map(|(_, r)| r).unwrap_or(area);
                                let tab_h = s.tab_h();
                                let content_y = pane_rect.y + tab_h;
                                let cw = s.glyphs.cw;
                                let lh = s.glyphs.lh;
                                let mx = s.mouse_x as i32;
                                let my = s.mouse_y as i32;
                                let mut mod_bits: u8 = 0;
                                if s.mods.shift_key()   { mod_bits |= 4; }
                                if s.mods.alt_key()     { mod_bits |= 8; }
                                if s.mods.control_key() { mod_bits |= 16; }
                                let cb_base: u8 = if dy < 0 { 64 } else { 65 };
                                let cb = cb_base | mod_bits;
                                let n = (dy.unsigned_abs() as usize).min(3);
                                let tp = s.term_panes.get(&tid).unwrap();
                                let sgr = tp.grid.mouse_sgr;
                                let pty = tp.input();
                                let term_col = ((mx - pane_rect.x) / cw)
                                    .clamp(0, tp.grid.cols as i32 - 1) as usize;
                                let term_row = ((my - content_y) / lh)
                                    .clamp(0, tp.grid.rows as i32 - 1) as usize;
                                for _ in 0..n {
                                    let bytes = terminal::encode_mouse(term_col, term_row, cb, true, sgr);
                                    pty.write(&bytes);
                                }
                            } else {
                                let tp = s.term_panes.get_mut(&tid).unwrap();
                                let sb = tp.grid.scrollback.len();
                                if dy < 0 {
                                    tp.grid.scroll_offset = (tp.grid.scroll_offset + (-dy) as usize).min(sb);
                                } else {
                                    tp.grid.scroll_offset = tp.grid.scroll_offset.saturating_sub(dy as usize);
                                }
                                s.term_sel = None; // selection positions are visual-row relative
                            }
                            { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        }
                    }
                    PaneKind::MarkdownPreview => {
                        if dy != 0 {
                            let source_buf_id = s.md_panes[&s.active_pane].source_buf_id;
                            let total = s.panes.values()
                                .find_map(|p| p.tabs.iter()
                                    .find(|t| t.buf_id == source_buf_id)
                                    .map(|t| t.text.len_lines()))
                                .unwrap_or(1);
                            let mp = s.md_panes.get_mut(&s.active_pane).unwrap();
                            let max_scroll = total.saturating_sub(1);
                            if dy > 0 {
                                mp.scroll = (mp.scroll + dy as usize).min(max_scroll);
                            } else {
                                mp.scroll = mp.scroll.saturating_sub((-dy) as usize);
                            }
                            { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        }
                    }
                    PaneKind::Editor => {
                        let is_settings = s.panes.get(&s.active_pane)
                            .and_then(|p| p.tabs.get(p.active))
                            .map_or(false, |t| t.kind == TabKind::Settings);
                        if is_settings {
                            if dy != 0 {
                                let lh = s.glyphs.lh as usize;
                                let pane_rect = s.active_pane_rect();
                                let visible_h = (pane_rect.h - s.tab_h()).max(0) as usize;
                                let content_h = if s.settings.undo_limit.is_some() { 34 * lh + 148 } else { 33 * lh + 144 };
                                let max_scroll = content_h.saturating_sub(visible_h) / lh + 1;
                                let t = s.tab_mut();
                                if dy > 0 {
                                    t.scroll = (t.scroll + dy as usize).min(max_scroll);
                                } else {
                                    t.scroll = t.scroll.saturating_sub((-dy) as usize);
                                }
                                { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                            }
                        } else {
                            if dy != 0 { s.scroll_by(dy); }
                            if dx != 0 { s.hscroll_by(dx); }
                            if dx != 0 || dy != 0 { { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); } }
                        }
                    }
                }
            }

            WindowEvent::KeyboardInput { mut event, .. } if event.state == ElementState::Pressed => {
                dlog!("[input] {:?}", event.logical_key);
                s.status_msg = None;
                // Windows reports AltGr as Ctrl+Alt: a character typed with it is text,
                // not a Ctrl/Alt shortcut (nor a control code for the terminal). The
                // next ModifiersChanged (releasing AltGr) restores the real state.
                if is_altgr_text(&event, s.mods) {
                    s.mods.remove(ModifiersState::CONTROL | ModifiersState::ALT);
                }
                let ctrl  = s.mods.control_key();
                // App shortcuts use `cmd`: Command on macOS, Control on Linux/Windows (where
                // `cmd` and `ctrl` are then both set; the places where Linux and
                // macOS conventions differ check MACOS).
                let cmd   = primary_mod(s.mods);
                let alt   = s.mods.alt_key();
                let shift = s.mods.shift_key();
                // Unshifted shortcut letters are matched lowercase: with Caps Lock on,
                // Ctrl+S arrives as "S" and missed every binding.
                if (cmd || ctrl) && !shift {
                    if let Key::Character(c) = &event.logical_key {
                        if c.len() == 1 && c.chars().all(|ch| ch.is_ascii_uppercase()) {
                            event.logical_key = Key::Character(c.to_ascii_lowercase().into());
                        }
                    }
                }
                // Whether the terminal is what's being typed into, decided before any
                // handler moves focus. On Linux it then owns plain Ctrl+<letter>
                // (readline's Ctrl+P/B/G/W…): app shortcuts there take Shift
                // (Ctrl+Shift+W/C/V/Q) or aren't letters (Ctrl+`, Ctrl+=).
                let term_focused = terminal_has_keyboard(s);
                let shell_keys = !MACOS && term_focused;

                // Ctrl+Q — quit (macOS gets Cmd+Q from the app menu)
                if !MACOS && cmd && (shift || !shell_keys)
                    && matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("q"))
                {
                    if request_quit(s) {
                        s.panes.clear();
                        el.exit();
                        return;
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // LSP manager panel captures all keys while open.
                if s.lsp_panel.is_some() {
                    let n = s.lsp_panel.as_ref().map_or(0, |p| p.entries.len());
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => { s.lsp_panel = None; }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(p) = s.lsp_panel.as_mut() { p.selected = p.selected.saturating_sub(1); }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(p) = s.lsp_panel.as_mut() {
                                if n > 0 { p.selected = (p.selected + 1).min(n - 1); }
                            }
                        }
                        Key::Named(NamedKey::Enter) => lsp_panel_go_to_window(s),
                        Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) => lsp_panel_stop(s),
                        Key::Character(c) if !cmd && !ctrl && c.as_str() == "g" => lsp_panel_go_to_window(s),
                        Key::Character(c) if !cmd && !ctrl && c.as_str() == "m" => lsp_panel_move_here(s),
                        Key::Character(c) if !cmd && !ctrl && c.as_str() == "x" => lsp_panel_stop(s),
                        _ => {}
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Cmd+, / Cmd+. — cursor history navigation (use physical_key to avoid macOS ≤/≥ chars)
                if cmd && !alt && event.physical_key == PhysicalKey::Code(KeyCode::Comma) {
                    cursor_go_back(s);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }
                if cmd && !alt && event.physical_key == PhysicalKey::Code(KeyCode::Period) {
                    cursor_go_fwd(s);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Ctrl+` — open a new terminal pane (works from any pane kind)
                if ctrl && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "`") {
                    open_terminal_pane(s);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Cmd+P — quick file finder
                if cmd && !shift && !shell_keys && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "p") {
                    open_quick_finder(s, self.proxy.clone());
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Cmd+Shift+P — command palette (unified quick finder in command mode)
                if cmd && shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "p" | "P")) {
                    open_quick_finder(s, self.proxy.clone());
                    s.quick_finder.query  = ">".to_string();
                    s.quick_finder.cursor = 1;
                    refilter_quick_finder(s);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Cmd+G — go to line in current file. While the editor's find bar is
                // open with a query it means "next match" instead (editor handler).
                let find_next = !s.quick_finder.open && active_editor_tab(s).is_some()
                    && s.find().open && !s.find().query.is_empty();
                if cmd && !shift && !find_next && !shell_keys
                    && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "g" | "G"))
                {
                    let pane_id = s.active_pane;
                    let (preview_pane_id, preview_tab_idx, preview_saved_char) =
                        if let Some(pane) = s.panes.get(&pane_id) {
                            if pane.kind == PaneKind::Editor {
                                let tidx = pane.active;
                                let saved = pane.tabs.get(tidx)
                                    .and_then(|t| t.cursors.first())
                                    .map(|c| c.head);
                                (Some(pane_id), Some(tidx), saved)
                            } else { (None, None, None) }
                        } else { (None, None, None) };
                    open_quick_finder(s, self.proxy.clone());
                    s.quick_finder.query               = ":".to_string();
                    s.quick_finder.cursor              = 1;
                    s.quick_finder.preview_pane_id     = preview_pane_id;
                    s.quick_finder.preview_tab_idx     = preview_tab_idx;
                    s.quick_finder.preview_saved_char  = preview_saved_char;
                    refilter_quick_finder(s);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Cmd+B — toggle left panel visibility
                if cmd && !shift && !shell_keys && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "b" | "B")) {
                    if s.explorer.is_some() {
                        s.left_panel_visible = !s.left_panel_visible;
                        // A hidden sidebar must not keep the keyboard.
                        if !s.left_panel_visible { blur_sidebar(s); }
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                }

                // Cmd+Shift+M — open/toggle markdown preview
                if cmd && shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "m" | "M")) {
                    open_markdown_preview(s);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Cmd+Shift+F — global find panel
                if cmd && shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "f" | "F")) {
                    if s.explorer.is_none() {
                        let root = std::env::current_dir().unwrap_or_default();
                        let root_vpath = VPath::Local(root);
                        s.explorer = Some(FileExplorer::new(root_vpath.clone()));
                        s.explorer_w = ((200.0 * s.font_size / FONT_PX).round() as i32).clamp(80, 600);
                        start_lsp_for_project(s, &root_vpath);
                    }
                    blur_sidebar(s);
                    s.left_view = LeftView::GlobalSearch;
                    s.left_panel_visible = true; // typing into a hidden field went nowhere
                    s.global_find.focus = GlobalFindFocus::Query;
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Cmd+= / Cmd++ / Cmd+- / Cmd+0 — font size, from any pane
                if cmd && !alt {
                    let size = match &event.logical_key {
                        Key::Character(c) if matches!(c.as_str(), "=" | "+") => Some(s.font_size + 2.0),
                        Key::Character(c) if !shift && c.as_str() == "-" => Some(s.font_size - 2.0),
                        Key::Character(c) if !shift && c.as_str() == "0" => Some(FONT_PX),
                        _ => None,
                    };
                    if let Some(size) = size {
                        set_font_size(s, size);
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                }

                // Quick finder / command palette input routing (unified)
                if s.quick_finder.open {
                    let is_cmd_mode  = s.quick_finder.query.starts_with('>');
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => quick_finder_cancel(s),
                        Key::Named(NamedKey::ArrowUp) => {
                            s.quick_finder.selected = s.quick_finder.selected.saturating_sub(1);
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            let n = if is_cmd_mode {
                                s.quick_finder.filtered_commands.len()
                            } else {
                                s.quick_finder.filtered.len()
                            };
                            if n > 0 { s.quick_finder.selected = (s.quick_finder.selected + 1).min(n - 1); }
                        }
                        Key::Named(NamedKey::Enter) => {
                            if quick_finder_accept(s) {
                                s.panes.clear();
                                el.exit();
                                return;
                            }
                        }
                        key => {
                            if let Key::Character(c) = key {
                                if !cmd && !ctrl { for ch in c.chars() { s.glyphs.load(ch); } }
                            }
                            let prev = s.quick_finder.query.clone();
                            {
                                let qf = &mut s.quick_finder;
                                input_field_edit(&mut qf.query, &mut qf.cursor, &mut qf.sel_anchor,
                                                 key, cmd, alt, ctrl, shift);
                            }
                            // If the query just transitioned to ':' mode, save preview cursor state
                            if !prev.starts_with(':') && s.quick_finder.query.starts_with(':')
                                && s.quick_finder.preview_saved_char.is_none()
                            {
                                let pane_id = s.active_pane;
                                if let Some(pane) = s.panes.get(&pane_id) {
                                    if pane.kind == PaneKind::Editor {
                                        let tidx = pane.active;
                                        let saved = pane.tabs.get(tidx)
                                            .and_then(|t| t.cursors.first())
                                            .map(|c| c.head);
                                        s.quick_finder.preview_pane_id    = Some(pane_id);
                                        s.quick_finder.preview_tab_idx    = Some(tidx);
                                        s.quick_finder.preview_saved_char = saved;
                                    }
                                }
                            }
                            // If the query just left ':' mode (user backspaced), restore cursor
                            if prev.starts_with(':') && !s.quick_finder.query.starts_with(':') {
                                if let Some(saved) = s.quick_finder.preview_saved_char {
                                    let pid  = s.quick_finder.preview_pane_id;
                                    let tidx = s.quick_finder.preview_tab_idx;
                                    if let (Some(pid), Some(tidx)) = (pid, tidx) {
                                        if let Some(pane) = s.panes.get_mut(&pid) {
                                            if let Some(tab) = pane.tabs.get_mut(tidx) {
                                                tab.cursors = vec![Cursor::new(saved)];
                                            }
                                        }
                                    }
                                }
                                s.quick_finder.preview_pane_id    = None;
                                s.quick_finder.preview_tab_idx    = None;
                                s.quick_finder.preview_saved_char = None;
                            }
                            if s.quick_finder.query != prev { refilter_quick_finder(s); }
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Context menu keyboard (Escape to close)
                if s.context_menu.is_some() {
                    if matches!(&event.logical_key, Key::Named(NamedKey::Escape)) {
                        s.context_menu = None;
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // ── Sidebar inputs ──────────────────────────────────────────────────
                // Each only owns the keyboard while its view is on screen; app-level
                // shortcuts (save, close, switch tab…) still reach the pane handlers.

                // Tree search input routing
                if s.explorer.as_ref().map_or(false, |ex| ex.tree_search_focused)
                    && s.left_panel_visible && s.left_view == LeftView::FileTree
                    && !sidebar_lets_through(s, &event.logical_key, cmd, ctrl, shift)
                {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            if let Some(ex) = s.explorer.as_mut() {
                                ex.tree_search.clear();
                                ex.tree_search_focused = false;
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(ex) = s.explorer.as_mut() {
                                ex.tree_search_sel = ex.tree_search_sel.saturating_sub(1);
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(ex) = s.explorer.as_mut() {
                                let n = ex.tree_search_results.len();
                                if n > 0 { ex.tree_search_sel = (ex.tree_search_sel + 1).min(n - 1); }
                            }
                        }
                        Key::Named(NamedKey::Enter) => {
                            let path = s.explorer.as_ref()
                                .and_then(|ex| ex.tree_search_results.get(ex.tree_search_sel).cloned());
                            if let Some(path) = path {
                                if let Some(ex) = s.explorer.as_mut() { ex.tree_search_focused = false; }
                                push_cursor_history(s);
                                open_or_reuse_tab(s, VPath::Local(path));
                            }
                        }
                        key => {
                            if let Key::Character(c) = key {
                                if !cmd && !ctrl { for ch in c.chars() { s.glyphs.load(ch); } }
                            }
                            if let Some(ex) = s.explorer.as_mut() {
                                let prev = ex.tree_search.clone();
                                input_field_edit(
                                    &mut ex.tree_search, &mut ex.tree_search_cursor,
                                    &mut ex.tree_search_sel_anchor,
                                    key, cmd, alt, ctrl, shift);
                                if ex.tree_search != prev { refilter_tree_search(ex); }
                            }
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Git panel commit field input routing
                if s.explorer.is_some() && s.left_panel_visible && s.left_view == LeftView::Git
                    && s.git_panel.commit_focused
                    && !sidebar_lets_through(s, &event.logical_key, cmd, ctrl, shift)
                {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            s.git_panel.commit_focused = false;
                        }
                        Key::Named(NamedKey::Enter) => {
                            let can_commit = !s.git_panel.staged.is_empty()
                                && !s.git_panel.commit_msg.is_empty();
                            if can_commit {
                                let msg = s.git_panel.commit_msg.clone();
                                s.git_panel.commit_msg.clear();
                                s.git_panel.commit_cursor = 0;
                                s.git_panel.commit_focused = false;
                                run_git_op(s, vec!["commit".into(), "-m".into(), msg]);
                            }
                        }
                        key => {
                            if let Key::Character(c) = key {
                                if !cmd && !ctrl {
                                    for ch in c.chars() { s.glyphs.load(ch); }
                                }
                            }
                            let mut sel: Option<usize> = None;
                            input_field_edit(
                                &mut s.git_panel.commit_msg,
                                &mut s.git_panel.commit_cursor,
                                &mut sel,
                                key, cmd, alt, ctrl, shift,
                            );
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Global find panel input routing (when a field has focus)
                if s.explorer.is_some() && s.left_panel_visible && s.left_view == LeftView::GlobalSearch
                    && !matches!(s.global_find.focus, GlobalFindFocus::Results)
                    && !sidebar_lets_through(s, &event.logical_key, cmd, ctrl, shift)
                {
                    let focus = s.global_find.focus;
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => blur_sidebar(s),
                        Key::Named(NamedKey::Tab) => {
                            let nf = match focus {
                                GlobalFindFocus::Query   => if shift { GlobalFindFocus::Exclude } else { GlobalFindFocus::Replace },
                                GlobalFindFocus::Replace => if shift { GlobalFindFocus::Query }   else { GlobalFindFocus::Include },
                                GlobalFindFocus::Include => if shift { GlobalFindFocus::Replace } else { GlobalFindFocus::Exclude },
                                GlobalFindFocus::Exclude => if shift { GlobalFindFocus::Include } else { GlobalFindFocus::Query },
                                GlobalFindFocus::Results => GlobalFindFocus::Query,
                            };
                            s.global_find.focus = nf;
                            // Reset cursor to end of newly focused field
                            let new_len = match nf {
                                GlobalFindFocus::Query   => s.global_find.query.len(),
                                GlobalFindFocus::Replace => s.global_find.replace.len(),
                                GlobalFindFocus::Include => s.global_find.include_glob.len(),
                                GlobalFindFocus::Exclude => s.global_find.exclude_glob.len(),
                                GlobalFindFocus::Results => 0,
                            };
                            *match nf {
                                GlobalFindFocus::Query   => &mut s.global_find.cursor_query,
                                GlobalFindFocus::Replace => &mut s.global_find.cursor_replace,
                                GlobalFindFocus::Include => &mut s.global_find.cursor_include,
                                GlobalFindFocus::Exclude => &mut s.global_find.cursor_exclude,
                                GlobalFindFocus::Results => &mut s.global_find.cursor_query,
                            } = new_len;
                        }
                        Key::Named(NamedKey::Enter)
                            if matches!(focus, GlobalFindFocus::Query | GlobalFindFocus::Include | GlobalFindFocus::Exclude) =>
                        {
                            let root = s.explorer.as_ref()
                                .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                            if !s.global_find.query.is_empty() {
                                spawn_search(&mut s.global_find, root, s.proxy.clone());
                                // Up/Down/Enter now walk the results.
                                s.global_find.focus = GlobalFindFocus::Results;
                                s.sidebar_list_focused = true;
                            }
                        }
                        key => {
                            if let Key::Character(c) = key {
                                if !cmd && !ctrl { for ch in c.chars() { s.glyphs.load(ch); } }
                            }
                            let focus = s.global_find.focus;
                            let gf = &mut s.global_find;
                            let (field, cursor, sel) = match focus {
                                GlobalFindFocus::Query   => (&mut gf.query,        &mut gf.cursor_query,   &mut gf.sel_anchor_q),
                                GlobalFindFocus::Replace => (&mut gf.replace,      &mut gf.cursor_replace, &mut gf.sel_anchor_r),
                                GlobalFindFocus::Include => (&mut gf.include_glob, &mut gf.cursor_include, &mut gf.sel_anchor_inc),
                                GlobalFindFocus::Exclude => (&mut gf.exclude_glob, &mut gf.cursor_exclude, &mut gf.sel_anchor_exc),
                                GlobalFindFocus::Results => unreachable!(),
                            };
                            let changed = input_field_edit(field, cursor, sel, key, cmd, alt, ctrl, shift);
                            // Schedule live search on any change to search-relevant fields
                            if changed && s.global_find.live_search
                                && matches!(focus, GlobalFindFocus::Query | GlobalFindFocus::Include | GlobalFindFocus::Exclude)
                            {
                                s.global_find.search_fire_at =
                                    Some(Instant::now() + Duration::from_millis(300));
                            }
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Sidebar list navigation (Diagnostics, Search results). Only while the
                // list has focus: Up/Down/Enter walk it, Escape hands the keyboard back
                // to the active pane, and any other key goes to the pane (dropping focus).
                if s.sidebar_list_focused {
                    let list_on_screen = s.explorer.is_some() && s.left_panel_visible && match s.left_view {
                        LeftView::Diagnostics  => true,
                        LeftView::GlobalSearch => s.global_find.focus == GlobalFindFocus::Results,
                        _ => false,
                    };
                    if !list_on_screen { s.sidebar_list_focused = false; }
                }
                if s.sidebar_list_focused && !is_modifier_key(&event.logical_key)
                    && !sidebar_lets_through(s, &event.logical_key, cmd, ctrl, shift)
                {
                    let diagnostics = s.left_view == LeftView::Diagnostics;
                    let nav = matches!(&event.logical_key, Key::Named(
                        NamedKey::ArrowUp | NamedKey::ArrowDown | NamedKey::Enter | NamedKey::Escape));
                    if nav && !(cmd || ctrl || alt) {
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => s.sidebar_list_focused = false,
                            Key::Named(NamedKey::ArrowUp) if diagnostics => {
                                s.diag_panel_sel = s.diag_panel_sel.saturating_sub(1);
                            }
                            Key::Named(NamedKey::ArrowDown) if diagnostics => {
                                let total: usize = s.diagnostics.values().map(|v| v.len()).sum();
                                if total > 0 { s.diag_panel_sel = (s.diag_panel_sel + 1).min(total - 1); }
                            }
                            Key::Named(NamedKey::Enter) if diagnostics => {
                                let mut sev_items: Vec<(usize, VPath, usize)> = s.diagnostics.iter()
                                    .flat_map(|(path, diags)| {
                                        let sev_ord = |s: &DiagSeverity| match s { DiagSeverity::Error => 0, DiagSeverity::Warning => 1, _ => 2 };
                                        diags.iter().map(move |d| (sev_ord(&d.severity), path.clone(), d.line))
                                    })
                                    .collect();
                                sev_items.sort();
                                let idx = s.diag_panel_sel.min(sev_items.len().saturating_sub(1));
                                if let Some((_, path, line)) = sev_items.get(idx).cloned() {
                                    push_cursor_history(s);
                                    jump_to_path_line_col(s, path, line, 0);
                                }
                            }
                            Key::Named(NamedKey::ArrowUp) => {
                                s.global_find.selected = s.global_find.selected.saturating_sub(1);
                                global_find_reveal_selected(s);
                            }
                            Key::Named(NamedKey::ArrowDown) => {
                                let n = s.global_find.results.len();
                                s.global_find.selected = (s.global_find.selected + 1).min(n.saturating_sub(1));
                                global_find_reveal_selected(s);
                            }
                            Key::Named(NamedKey::Enter) => {
                                if let Some(r) = s.global_find.results.get(s.global_find.selected).cloned() {
                                    push_cursor_history(s);
                                    jump_to_path_line_col(s, r.path, r.line_num, r.match_col);
                                }
                            }
                            _ => {}
                        }
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                    s.sidebar_list_focused = false;
                    s.needs_redraw = true;
                }

                // Settings text field editing. The field lives in the Settings tab: once
                // another tab or pane is active, keep what was typed and stop capturing.
                if s.settings_edit_field.is_some() && !settings_tab_active(s) {
                    commit_settings_field(s);
                }
                if s.settings_edit_field.is_some() && !is_app_shortcut(&event.logical_key, cmd, ctrl, shift) {
                    let char_byte = |text: &str, ci: usize| text.char_indices().nth(ci).map_or(text.len(), |(i, _)| i);
                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) => commit_settings_field(s),
                        Key::Named(NamedKey::Escape) => { s.settings_edit_field = None; }
                        Key::Named(NamedKey::Backspace) => {
                            if s.settings_edit_cursor > 0 {
                                let byte_off = char_byte(&s.settings_edit_text, s.settings_edit_cursor - 1);
                                s.settings_edit_text.remove(byte_off);
                                s.settings_edit_cursor -= 1;
                            }
                        }
                        Key::Named(NamedKey::Delete) => {
                            if s.settings_edit_cursor < s.settings_edit_text.chars().count() {
                                let byte_off = char_byte(&s.settings_edit_text, s.settings_edit_cursor);
                                s.settings_edit_text.remove(byte_off);
                            }
                        }
                        Key::Named(NamedKey::ArrowLeft)  => {
                            s.settings_edit_cursor = s.settings_edit_cursor.saturating_sub(1);
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            let max = s.settings_edit_text.chars().count();
                            if s.settings_edit_cursor < max { s.settings_edit_cursor += 1; }
                        }
                        Key::Named(NamedKey::Home) => { s.settings_edit_cursor = 0; }
                        Key::Named(NamedKey::End)  => {
                            s.settings_edit_cursor = s.settings_edit_text.chars().count();
                        }
                        key => {
                            // With Cmd held winit still reports the letter as text, so
                            // shortcuts must never be inserted (Cmd+V used to type "v").
                            let txt: Option<String> = if cmd && matches!(key, Key::Character(c) if matches!(c.as_str(), "v" | "V")) {
                                clipboard_read().lines().next().map(str::to_owned)
                            } else if cmd || ctrl || (!MACOS && alt) {
                                None
                            } else {
                                event.text.as_deref().map(str::to_owned)
                            };
                            if let Some(txt) = txt.filter(|t| !t.is_empty() && !t.chars().any(|c| c.is_control())) {
                                for ch in txt.chars() { s.glyphs.load(ch); }
                                let byte_off = char_byte(&s.settings_edit_text, s.settings_edit_cursor);
                                s.settings_edit_text.insert_str(byte_off, &txt);
                                s.settings_edit_cursor += txt.chars().count();
                            }
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Cmd+W — close the active tab of any pane kind (LSP output panes hide).
                // In a focused terminal on Linux Ctrl+W is the shell's: Ctrl+Shift+W.
                if cmd && shift == shell_keys && matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("w")) {
                    if close_active_tab(s) {
                        s.panes.clear();
                        el.exit();
                        return;
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Terminal pane intercepts (before PTY forwarding)
                if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Terminal) {
                    // A shortcut let through from a sidebar field is not terminal input
                    // (e.g. Ctrl+S on its way to "save" must not send XOFF to the shell).
                    if !term_focused {
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                    let is_char = |k: &str| matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case(k));
                    let backspace = matches!(&event.logical_key, Key::Named(NamedKey::Backspace));
                    // Ctrl+Shift+5 — split: new terminal with same shell below
                    if ctrl && shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "5" | "%")) {
                        // If in a remote workspace, prefer SSH over duplicating the local shell.
                        let shell = if let Some(host) = active_workspace_ssh_host(s) {
                            terminal::TermCommand::Argv(ssh::terminal_argv(&host, None))
                        } else {
                            let p = &s.panes[&s.active_pane];
                            p.term_ids.get(p.active)
                                .and_then(|&tid| s.term_panes.get(&tid).map(|tp| tp.command.clone()))
                                .filter(|c| *c != terminal::TermCommand::Argv(Vec::new())) // not an LSP log
                                .unwrap_or_else(|| terminal::TermCommand::shell(&s.settings.terminal_shell))
                        };
                        let pane_id = s.next_pane_id; s.next_pane_id += 1;
                        let term_id = s.next_pane_id; s.next_pane_id += 1;
                        let area = s.pane_area();
                        let cols = (area.w / s.glyphs.cw).max(1) as usize;
                        let rows = ((area.h / 2) / s.glyphs.lh).max(1) as usize;
                        let proxy = s.proxy.clone();
                        let tp = terminal::spawn_terminal(term_id, cols, rows, proxy, shell, None);
                        s.term_panes.insert(term_id, tp);
                        let new_pane = Pane { id: pane_id, kind: PaneKind::Terminal, tabs: vec![],
                                              term_ids: vec![term_id], active: 0, find: FindBar::new() };
                        s.panes.insert(pane_id, new_pane);
                        let cur = s.active_pane;
                        let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                        s.pane_tree = insert_pane(old_tree, cur, pane_id, DropZone::Bottom);
                        s.active_pane = pane_id;
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                    // Copy the selection: Cmd+C on macOS; on Linux Ctrl+Shift+C, or Ctrl+C
                    // while text is selected if terminal copy/paste is enabled.
                    let copy_key = is_char("c") && !alt
                        && if MACOS { cmd } else { ctrl && (shift || s.settings.term_copy_paste) };
                    if copy_key {
                        if let Some(sel) = s.term_sel.clone() {
                            if !sel.is_empty() {
                                let pane_id = s.active_pane;
                                let tid = s.panes[&pane_id].term_ids.get(s.panes[&pane_id].active).copied();
                                if let Some(tid) = tid {
                                    if let Some(tp) = s.term_panes.get(&tid) {
                                        let rows = tp.grid.visible_rows();
                                        let mut text = String::new();
                                        let (r0, c0, r1, c1) = sel.normalized();
                                        for vi in r0..=r1 {
                                            if vi >= rows.len() { break; }
                                            let row = &rows[vi];
                                            let col_start = if vi == r0 { c0 } else { 0 };
                                            let col_end   = if vi == r1 { c1 + 1 } else { row.len() };
                                            let col_end   = col_end.min(row.len());
                                            // Trim trailing spaces from non-last rows
                                            let slice: String = row[col_start..col_end].iter()
                                                .map(|c| c.ch).collect();
                                            let trimmed = if vi < r1 { slice.trim_end().to_owned() } else { slice };
                                            text.push_str(&trimmed);
                                            if vi < r1 { text.push('\n'); }
                                        }
                                        clipboard_write(&text);
                                    }
                                }
                                return;
                            }
                        }
                        // No selection: Cmd+C / Ctrl+Shift+C do nothing, while plain Ctrl+C
                        // goes on to the shell as an interrupt.
                        if MACOS || shift { return; }
                    }
                    // Paste: Cmd+V on macOS when terminal copy/paste is enabled; on Linux
                    // Ctrl+Shift+V, or Ctrl+V when enabled.
                    let paste_key = is_char("v") && !alt && if MACOS {
                        cmd && s.settings.term_copy_paste
                    } else {
                        ctrl && (shift || s.settings.term_copy_paste)
                    };
                    if paste_key {
                        s.term_sel = None;
                        let text = clipboard_read();
                        if !text.is_empty() {
                            let p = &s.panes[&s.active_pane];
                            if let Some(&tid) = p.term_ids.get(p.active) {
                                if let Some(tp) = s.term_panes.get(&tid) {
                                    let bytes = text.as_bytes();
                                    tp.write_input(bytes);
                                }
                            }
                        }
                        return;
                    }
                    // Cmd+Backspace (Linux: Ctrl+Shift+Backspace) → send \x15 (kill to line start) when enabled
                    if s.settings.term_cmd_bs && backspace && if MACOS { cmd } else { ctrl && shift } {
                        let p = &s.panes[&s.active_pane];
                        if let Some(&tid) = p.term_ids.get(p.active) {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                tp.write_input(b"\x15");
                            }
                        }
                        return;
                    }
                    // Alt/Option+Backspace → send \x17 (delete previous word) when enabled;
                    // on Linux Ctrl+Backspace always does, as it does in the editor.
                    if backspace && ((s.settings.term_alt_bs && alt) || (!MACOS && ctrl && !shift && !alt)) {
                        let p = &s.panes[&s.active_pane];
                        if let Some(&tid) = p.term_ids.get(p.active) {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                tp.write_input(b"\x17");
                            }
                        }
                        return;
                    }
                    // Forward all other key events to the active PTY.
                    // Guard: modifier-only keypresses (Cmd, Ctrl, Alt, Shift, etc.) must
                    // NOT clear the terminal selection. On macOS, pressing Cmd alone fires
                    // a KeyboardInput event; clearing term_sel here would destroy the
                    // selection before the user can press C for Cmd+C.
                    // Cmd (Super) combos are app shortcuts, never terminal input: winit
                    // still reports their letter as text, so an unbound one such as
                    // Cmd+S or Cmd+1 used to type "s"/"1" into the shell.
                    if is_modifier_key(&event.logical_key) || s.mods.super_key() {
                        s.needs_redraw = true;
                        self.dirty.store(true, Ordering::Release);
                        return;
                    }
                    s.term_sel = None;
                    let target = {
                        let pane = &s.panes[&s.active_pane];
                        pane.term_ids.get(pane.active).and_then(|tid| {
                            s.term_panes.get(tid)
                                .map(|term| (term.input(), term.grid.application_cursor))
                        })
                    };
                    let application_cursor = target.map_or(false, |(_, mode)| mode);
                    let mut bytes = None;
                    if !MACOS {
                        // xterm conventions: modifier parameters on arrows/Home/End
                        // (Ctrl+Left = word left in readline).
                        bytes = terminal::encode_modified_cursor_key(&event.logical_key, s.mods);
                    }
                    let bytes = bytes.or_else(|| terminal::encode_key(
                        &event.logical_key,
                        s.mods,
                        event.text.as_deref(),
                        application_cursor,
                    )).map(|b| {
                        // Linux: Alt is Meta and prefixes ESC (Alt+B/F/., Alt+Backspace).
                        let meta = !MACOS && alt
                            && matches!(&event.logical_key, Key::Character(_) | Key::Named(NamedKey::Backspace));
                        if meta { [b"\x1b".as_slice(), &b].concat() } else { b }
                    });
                    if let Some(bytes) = bytes {
                        if let Some((pty, _)) = target {
                            pty.write(&bytes);
                        }
                    }
                    return;
                }

                // MarkdownPreview pane: read-only (Cmd+W is handled above)
                if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::MarkdownPreview) {
                    return;
                }

                // A visible find bar only intercepts keys while its input owns focus.
                let find_handled = if s.find().open && s.find().input_focused {
                    let key = &event.logical_key;
                    match key {
                        Key::Named(NamedKey::Escape) => {
                            let f = s.find_mut();
                            f.open = false;
                            f.input_focused = false;
                            true
                        }
                        Key::Named(NamedKey::Tab) => {
                            if s.find().replace_open {
                                let nf = if s.find().focus == FindFocus::Query { FindFocus::Replace } else { FindFocus::Query };
                                let new_len = if nf == FindFocus::Query { s.find().query.len() } else { s.find().replace.len() };
                                let f = s.find_mut();
                                f.focus = nf;
                                if nf == FindFocus::Query { f.cursor_query = new_len; }
                                else { f.cursor_replace = new_len; }
                            }
                            true
                        }
                        Key::Named(NamedKey::Enter) => {
                            if s.find().focus == FindFocus::Replace && s.find().replace_open {
                                replace_current(s);
                            } else {
                                find_step(s, shift);
                            }
                            true
                        }
                        key => {
                            if let Key::Character(c) = key {
                                if !cmd && !ctrl { for ch in c.chars() { s.glyphs.load(ch); } }
                            }
                            let f = s.find_mut();
                            let focus = f.focus;
                            let (field, cursor, sel) = match focus {
                                FindFocus::Query   => (&mut f.query,   &mut f.cursor_query,   &mut f.sel_anchor_q),
                                FindFocus::Replace => (&mut f.replace, &mut f.cursor_replace, &mut f.sel_anchor_r),
                            };
                            input_field_edit(field, cursor, sel, key, cmd, alt, ctrl, shift)
                        }
                    }
                } else { false };
                if find_handled {
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                } else {
                    // Block text editing in settings and git-diff tabs: neither has a
                    // real buffer (a diff tab's path names the diffed file, so typing
                    // and saving there would overwrite it).
                    let active_tab_kind = s.panes.get(&s.active_pane)
                        .and_then(|p| p.tabs.get(p.active))
                        .map(|t| t.kind.clone());
                    let active_tab_is_settings = active_tab_kind == Some(TabKind::Settings);
                    // Shared documents the guest's role may not edit are read-only too.
                    let collab_ro = s.panes.get(&s.active_pane)
                        .and_then(|p| p.tabs.get(p.active))
                        .map_or(false, |t| collab_tab_read_only(s, t));
                    let read_only = active_tab_kind != Some(TabKind::Editor) || collab_ro;

                    // Main editor keyboard handling
                    let handled =
                        if read_only && (cmd || alt || ctrl) && matches!(&event.logical_key,
                            Key::Character(c) if if collab_ro {
                                matches!(c.as_str(), "x" | "v" | "z" | "Z" | "y" | "h")
                            } else {
                                matches!(c.as_str(), "a" | "x" | "v" | "z" | "Z" | "y" | "d" | "l" | "L" | "f" | "h" | "g" | "G")
                            })
                        {
                            true // editing / selection shortcuts do nothing in read-only tabs
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "a") {
                            let len = s.tab().text.len_chars();
                            s.tab_mut().cursors = vec![Cursor { head: len, tail: 0 }];
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "c") {
                            {
                                let order = s.cursor_order_ltr();
                                let texts: Vec<String> = order.iter()
                                    .filter(|&&i| s.tab().cursors[i].has_sel())
                                    .map(|&i| { let lo = s.tab().cursors[i].lo(); let hi = s.tab().cursors[i].hi(); s.tab().text.slice(lo..hi).chars().collect() })
                                    .collect();
                                if !texts.is_empty() { clipboard_set(&texts.join("\n")); }
                            }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "x") {
                            {
                                let order = s.cursor_order_ltr();
                                let texts: Vec<String> = order.iter()
                                    .filter(|&&i| s.tab().cursors[i].has_sel())
                                    .map(|&i| { let lo = s.tab().cursors[i].lo(); let hi = s.tab().cursors[i].hi(); s.tab().text.slice(lo..hi).chars().collect() })
                                    .collect();
                                if !texts.is_empty() {
                                    clipboard_set(&texts.join("\n"));
                                    s.push_undo(false);
                                    let mut delta: isize = 0;
                                    for &i in &order {
                                        if s.tab().cursors[i].has_sel() {
                                            let lo = apply_delta(s.tab().cursors[i].lo(), delta, s.tab().text.len_chars());
                                            let hi = apply_delta(s.tab().cursors[i].hi(), delta, s.tab().text.len_chars());
                                            s.tab_mut().text.remove(lo..hi);
                                            s.tab_mut().cursors[i] = Cursor::new(lo);
                                            delta -= (hi - lo) as isize;
                                            s.tab_mut().dirty = true;
                                        }
                                    }
                                    s.dedup_cursors();
                                    s.ensure_visible();
                                }
                            }
                            true
                        } else if cmd && !shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "z") {
                            s.undo();
                            true
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "z" | "Z")) {
                            s.redo();
                            true
                        } else if !MACOS && cmd && !shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "y") {
                            s.redo(); // Ctrl+Y, the Linux redo alongside Ctrl+Shift+Z
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "v") {
                            if let Some(text) = clipboard_get() {
                                for ch in text.chars() { s.glyphs.load(ch); }
                                paste_text(s, &text);
                            }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "o") {
                            if let Some(path) = open_file_dialog() {
                                open_or_reuse_tab(s, VPath::Local(path));
                            } else if dialog_tool_missing() {
                                s.status_msg = Some("Install zenity or kdialog for file dialogs".to_owned());
                            }
                            true
                        } else if (ctrl || cmd) && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "s") {
                            execute_command(s, CommandAction::Save);
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "t" | "n" | "N")) {
                            if s.pane().kind == PaneKind::Editor {
                                let new_buf_id = s.next_buf_id;
                                s.next_buf_id += 1;
                                let pane = s.pane_mut();
                                pane.tabs.push(Tab::untitled(new_buf_id));
                                pane.active = pane.tabs.len() - 1;
                                s.reset_blink();
                            }
                            true
                        } else if cmd && !shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "\\") {
                            if s.pane().kind == PaneKind::Editor {
                                let new_id = s.next_pane_id;
                                s.next_pane_id += 1;
                                let mut new_pane = Pane::new(new_id, 0); // buf_id preserved via clone
                                new_pane.tabs = vec![s.pane().tab().clone()];
                                s.panes.insert(new_id, new_pane);
                                let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                                s.pane_tree = insert_pane(old_tree, s.active_pane, new_id, DropZone::Right);
                                s.active_pane = new_id;
                            }
                            true
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "_") {
                            if s.pane().kind == PaneKind::Editor {
                                let new_id = s.next_pane_id;
                                s.next_pane_id += 1;
                                let mut new_pane = Pane::new(new_id, 0); // buf_id preserved via clone
                                new_pane.tabs = vec![s.pane().tab().clone()];
                                s.panes.insert(new_id, new_pane);
                                let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                                s.pane_tree = insert_pane(old_tree, s.active_pane, new_id, DropZone::Bottom);
                                s.active_pane = new_id;
                            }
                            true
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "]" | "}")) {
                            if s.pane().kind == PaneKind::Editor && !s.pane().tabs.is_empty() {
                                let pane = s.pane_mut();
                                pane.active = (pane.active + 1) % pane.tabs.len();
                            }
                            true
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "[" | "{")) {
                            if s.pane().kind == PaneKind::Editor && !s.pane().tabs.is_empty() {
                                let n = s.pane().tabs.len();
                                let pane = s.pane_mut();
                                pane.active = pane.active.checked_sub(1).unwrap_or(n - 1);
                            }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "1"|"2"|"3"|"4"|"5"|"6"|"7"|"8"|"9")) {
                            if s.pane().kind == PaneKind::Editor {
                                if let Key::Character(c) = &event.logical_key {
                                    if let Ok(n) = c.as_str().parse::<usize>() {
                                        let pane = s.pane_mut();
                                        if n >= 1 && n - 1 < pane.tabs.len() { pane.active = n - 1; }
                                    }
                                }
                            }
                            true
                        } else if ctrl && matches!(&event.logical_key, Key::Named(NamedKey::Tab)) {
                            let n = s.pane().tabs.len();
                            let pane = s.pane_mut();
                            if shift {
                                pane.active = pane.active.checked_sub(1).unwrap_or(n - 1);
                            } else {
                                pane.active = (pane.active + 1) % n;
                            }
                            true
                        } else if ctrl && matches!(&event.logical_key, Key::Named(NamedKey::PageUp | NamedKey::PageDown)) {
                            // Ctrl+PageUp / Ctrl+PageDown — previous / next tab
                            let n = s.pane().tabs.len();
                            let back = matches!(&event.logical_key, Key::Named(NamedKey::PageUp));
                            let pane = s.pane_mut();
                            pane.active = if back { pane.active.checked_sub(1).unwrap_or(n - 1) } else { (pane.active + 1) % n };
                            true
                        }
                        // ── Find / Replace ────────────────────────────────────────────────────
                        else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "f") {
                            blur_sidebar(s);
                            let query = s.tab().sel_text().unwrap_or_default();
                            let f = s.find_mut();
                            f.open = true;
                            f.input_focused = true;
                            f.replace_open = false;
                            f.focus = FindFocus::Query;
                            f.query = query;
                            f.cursor_query = f.query.len();
                            f.sel_anchor_q = None;
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "h") {
                            blur_sidebar(s);
                            let query = s.tab().sel_text().unwrap_or_default();
                            let f = s.find_mut();
                            f.open = true;
                            f.input_focused = true;
                            f.replace_open = true;
                            f.focus = FindFocus::Query;
                            f.query = query;
                            f.cursor_query = f.query.len();
                            f.sel_anchor_q = None;
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "g" | "G")) {
                            // Cmd+Shift+G = previous match (Cmd+G is routed here only while
                            // the find bar is open; otherwise it is "go to line").
                            if !s.find().query.is_empty() { find_step(s, shift); }
                            true
                        } else if matches!(&event.logical_key, Key::Named(NamedKey::F3)) {
                            // F3 / Shift+F3 — next / previous match (Linux convention)
                            if !s.find().query.is_empty() { find_step(s, shift); }
                            true
                        }
                        // ── Multi-cursor ──────────────────────────────────────────────────────
                        else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "d") {
                            if !s.tab().primary().has_sel() {
                                let pos = s.tab().primary().head;
                                let (lo, hi) = word_bounds_at(s.tab(), pos);
                                if lo < hi {
                                    *s.tab_mut().primary_mut() = Cursor { head: hi, tail: lo };
                                }
                            }
                            let query = s.tab().sel_text().unwrap_or_default();
                            if !query.is_empty() {
                                let (cs, ww) = selection_match_flags(s.tab());
                                let ms = find_matches(&s.tab().text, &query, cs, ww);
                                let after = s.tab().primary().hi();
                                let idx = ms.iter().position(|&(lo, _)| lo >= after).unwrap_or(0);
                                if let Some(&(mlo, mhi)) = ms.get(idx) {
                                    if (mlo, mhi) != (s.tab().primary().lo(), s.tab().primary().hi()) {
                                        s.tab_mut().cursors.push(Cursor { head: mhi, tail: mlo });
                                        s.dedup_cursors();
                                    }
                                }
                            }
                            true
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "l" | "L")) {
                            // If no selection, select word at primary cursor first.
                            if !s.tab().primary().has_sel() {
                                let pos = s.tab().primary().head;
                                let (lo, hi) = word_bounds_at(s.tab(), pos);
                                if lo < hi {
                                    *s.tab_mut().primary_mut() = Cursor { head: hi, tail: lo };
                                }
                            }
                            if let Some(query) = s.tab().sel_text().filter(|t| !t.is_empty()) {
                                let (cs, ww) = selection_match_flags(s.tab());
                                let ms = find_matches(&s.tab().text, &query, cs, ww);
                                if !ms.is_empty() {
                                    s.tab_mut().cursors = ms.into_iter()
                                        .map(|(lo, hi)| Cursor { head: hi, tail: lo })
                                        .collect();
                                    s.dedup_cursors();
                                }
                            }
                            true
                        }
                        // ── Escape: cancel drag / collapse multi-cursor / close find ──────────
                        else if matches!(&event.logical_key, Key::Named(NamedKey::Escape)) {
                            if s.drag.is_some() {
                                s.drag = None;
                                true
                            } else if s.tab().cursors.len() > 1 {
                                let head = s.tab().primary().head;
                                s.tab_mut().cursors = vec![Cursor::new(head)];
                                true
                            } else if s.find().open {
                                let f = s.find_mut();
                                f.open = false;
                                f.input_focused = false;
                                true
                            } else { false }
                        } else if alt && shift && event.physical_key == PhysicalKey::Code(KeyCode::KeyF) {
                            execute_command(s, CommandAction::FormatDocument);
                            true
                        } else if alt && shift && event.physical_key == PhysicalKey::Code(KeyCode::KeyO) {
                            execute_command(s, CommandAction::OrganizeImports);
                            true
                        } else if ctrl && matches!(&event.logical_key, Key::Character(_)) {
                            false
                        } else if collab_ro && (matches!(&event.logical_key,
                            Key::Named(NamedKey::Backspace | NamedKey::Delete | NamedKey::Enter | NamedKey::Tab | NamedKey::Space))
                            || (matches!(&event.logical_key, Key::Character(_)) && !cmd && !ctrl && (MACOS || !alt)))
                        {
                            s.status_msg = Some("This shared file is read-only for your role".to_owned());
                            false
                        } else if read_only && !active_tab_is_settings && !collab_ro {
                            // Git diff tab: keys only scroll.
                            match &event.logical_key {
                                Key::Named(NamedKey::ArrowUp)   => s.scroll_by(-1),
                                Key::Named(NamedKey::ArrowDown) => s.scroll_by(1),
                                Key::Named(NamedKey::PageUp)    => s.scroll_by(-10),
                                Key::Named(NamedKey::PageDown)  => s.scroll_by(10),
                                _ => {}
                            }
                            false
                        } else if active_tab_is_settings {
                            match &event.logical_key {
                                Key::Named(NamedKey::ArrowUp) | Key::Named(NamedKey::PageUp) => {
                                    s.tab_mut().scroll = s.tab().scroll.saturating_sub(if matches!(&event.logical_key, Key::Named(NamedKey::PageUp)) { 5 } else { 1 });
                                }
                                Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::PageDown) => {
                                    let lh = s.glyphs.lh as usize;
                                    let pane_rect = s.active_pane_rect();
                                    let visible_h = (pane_rect.h - s.tab_h()).max(0) as usize;
                                    let content_h = if s.settings.undo_limit.is_some() { 34 * lh + 148 } else { 33 * lh + 144 };
                                    let max_scroll = content_h.saturating_sub(visible_h) / lh + 1;
                                    let step = if matches!(&event.logical_key, Key::Named(NamedKey::PageDown)) { 5 } else { 1 };
                                    s.tab_mut().scroll = (s.tab().scroll + step).min(max_scroll);
                                }
                                _ => {}
                            }
                            false
                        } else {
                            match &event.logical_key {
                                // macOS: Cmd+arrows go to line/document ends, Option(/Ctrl)
                                // moves by word, Cmd+Option+Up/Down adds a cursor.
                                // Linux: Ctrl(/Alt) moves by word, Home/End and Ctrl+Home/End
                                // go to the ends, Ctrl+Up/Down scrolls, and Ctrl+Shift or
                                // Alt+Shift (or Ctrl+Alt) +Up/Down adds a cursor.
                                Key::Named(NamedKey::ArrowLeft) => {
                                    if MACOS && cmd   { s.move_home(shift); }
                                    else if alt||ctrl { s.move_word_left(shift); }
                                    else              { s.move_left(shift); }
                                }
                                Key::Named(NamedKey::ArrowRight) => {
                                    if MACOS && cmd   { s.move_end(shift); }
                                    else if alt||ctrl { s.move_word_right(shift); }
                                    else              { s.move_right(shift); }
                                }
                                Key::Named(NamedKey::ArrowUp) => {
                                    if (cmd && alt) || (!MACOS && shift && (ctrl || alt)) { s.add_cursor_above(); }
                                    else if MACOS && cmd { s.move_doc_start(shift); }
                                    else if ctrl && !MACOS { s.scroll_by(-1); }
                                    else                 { s.move_up(shift); }
                                }
                                Key::Named(NamedKey::ArrowDown) => {
                                    if (cmd && alt) || (!MACOS && shift && (ctrl || alt)) { s.add_cursor_below(); }
                                    else if MACOS && cmd { s.move_doc_end(shift); }
                                    else if ctrl && !MACOS { s.scroll_by(1); }
                                    else                 { s.move_down(shift); }
                                }
                                Key::Named(NamedKey::Home) => {
                                    if ctrl { s.move_doc_start(shift); } else { s.move_home(shift); }
                                }
                                Key::Named(NamedKey::End) => {
                                    if ctrl { s.move_doc_end(shift); } else { s.move_end(shift); }
                                }
                                // To the line start/end: Cmd+Backspace/Delete on macOS,
                                // Ctrl+Shift+Backspace/Delete on Linux (Ctrl alone = word).
                                Key::Named(NamedKey::Backspace) => {
                                    if (MACOS && cmd) || (!MACOS && ctrl && shift) { s.delete_to_line_start(); }
                                    else if alt||ctrl { s.delete_word_back(); }
                                    else              { s.backspace(); }
                                }
                                Key::Named(NamedKey::Delete) => {
                                    if (MACOS && cmd) || (!MACOS && ctrl && shift) { s.delete_to_line_end(); }
                                    else if alt||ctrl { s.delete_word_fwd(); }
                                    else              { s.delete_fwd(); }
                                }
                                Key::Named(NamedKey::Enter)    => s.insert_newline(),
                                Key::Named(NamedKey::Tab)      => if shift { s.dedent() } else { s.indent() },
                                Key::Named(NamedKey::PageUp)   => s.scroll_by(-10),
                                Key::Named(NamedKey::PageDown) => s.scroll_by(10),
                                Key::Named(NamedKey::F12) => {
                                    let action = if shift && cmd {
                                        CommandAction::FindReferences
                                    } else {
                                        CommandAction::GotoDefinition
                                    };
                                    execute_command(s, action);
                                }
                                _ => {
                                    // On Linux Alt+letter is a shortcut, not text (macOS
                                    // Option types accented/symbol characters).
                                    if !ctrl && !cmd && (MACOS || !alt) {
                                        if let Some(txt) = event.text.as_deref() {
                                            for ch in txt.chars() { s.glyphs.load(ch); }
                                            s.insert_str(txt);
                                        }
                                    }
                                }
                            }
                            s.reset_blink();
                            false
                        };

                    notify_lsp_change(s);
                    notify_collab_change(s);
                    send_collab_cursor_if_needed(s);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    let _ = handled;
                }
            }

            WindowEvent::RedrawRequested => {
                render(s);
            }

            _ => {}
        }
    }

    fn handle_about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let Some(s) = self.state.as_mut() {
            // Send any local edit not yet flushed by its handler (menus, palette
            // commands, mouse actions) to collab peers before the next event.
            if s.collab.is_some() && !s.panes.is_empty() {
                notify_collab_change(s);
                send_collab_cursor_if_needed(s);
            }
            // When vsync is off (no display link), flush dirty flag here so we get
            // one render per event batch rather than one per event.
            if !s.settings.vsync && s.needs_redraw {
                s.needs_redraw = false;
                s.win.request_redraw();
            }
            let next_wake = if let Some(fire_at) = s.global_find.search_fire_at {
                fire_at.min(s.cursor_blink)
            } else {
                s.cursor_blink
            };
            el.set_control_flow(ControlFlow::WaitUntil(next_wake));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }

    fn handle_user_event(&mut self, _el: &ActiveEventLoop, event: UserEvent) {
        let Some(s) = self.state.as_mut() else { return };
        if s.panes.is_empty() { return; }
        // Background events can arrive in bursts, especially PTY output. Mark
        // dirty here and let about_to_wait/display-link coalesce redraws.
        let mut schedule_redraw = true;
        match event {
            UserEvent::TermOutput { pane_id, data } => {
                schedule_redraw = false;
                if let Some(tp) = s.term_panes.get_mut(&pane_id) {
                    terminal::feed_bytes(tp, &data);
                    schedule_redraw = true;
                }
            }
            UserEvent::LspOutput { pane_id, data } => {
                // Protocol detection only: send `initialized` after receiving initialize response
                let mut initialized_key: Option<(Option<vpath::SshHost>, Lang)> = None;
                if let Some(srv) = s.lsp.servers.get_mut(&pane_id) {
                    if !srv.initialized {
                        if let Ok(text) = String::from_utf8(data) {
                            if lsp::is_initialize_response(&text) {
                                lsp::send_initialized(srv);
                                initialized_key = Some((srv.ssh_host.clone(), srv.lang));
                            }
                        }
                    }
                }
                if let Some(key) = initialized_key {
                    s.lsp_start_errors.remove(&key);
                    // Documents can only be sent after `initialized`: open them all now.
                    open_all_docs_for_server(s, key.1, key.0);
                }
            }
            UserEvent::LspDiagnostics { path, mut diagnostics } => {
                // Servers report UTF-16 columns; convert against the open buffer, and
                // file the result under the tab's own path (a remote tab may be
                // addressed as `~/…` while the server reports `/home/…`).
                let mut key = path.clone();
                if let Some((pid, ti)) = find_tab_for_path(s, &path) {
                    let tab = &s.panes[&pid].tabs[ti];
                    for d in &mut diagnostics {
                        d.col_start = utf16_to_char_col(&tab.text, d.line, d.col_start);
                        d.col_end   = utf16_to_char_col(&tab.text, d.line, d.col_end);
                    }
                    if let Some(p) = tab.path.clone() { key = p; }
                }
                s.diagnostics.insert(key, diagnostics);
            }
            UserEvent::LspResponse { server_id, id, result } => {
                let (kind, ssh_host) = s.lsp.servers.get_mut(&server_id)
                    .map(|srv| (srv.pending.remove(&id), srv.ssh_host.clone()))
                    .unwrap_or((None, None));
                match kind {
                    Some(lsp::PendingKind::Definition) => {
                        apply_goto_definition(s, &result, ssh_host);
                    }
                    Some(lsp::PendingKind::References) => {
                        apply_references(s, &result, ssh_host);
                    }
                    Some(lsp::PendingKind::Formatting { path, generation, save_after }) => {
                        let changed = apply_text_edits(s, &path, &result, Some(generation));
                        if changed && save_after {
                            if let Some((pid, ti)) = find_tab_for_path(s, &path) { save_tab(s, pid, ti); }
                        }
                    }
                    Some(lsp::PendingKind::OrganizeImports { path, generation, then_format, save_after }) => {
                        let changed = organize_imports_edits(&path, &result, ssh_host.as_ref())
                            .map_or(false, |edits| apply_text_edits(s, &path, &edits, Some(generation)));
                        if then_format {
                            // Formatting has to be computed against the post-import text.
                            if let Some((pid, ti)) = find_tab_for_path(s, &path) {
                                let gen_now = s.panes[&pid].tabs[ti].edit_generation;
                                if changed || gen_now == generation {
                                    notify_lsp_change_for(s, pid, ti);
                                    let lang = s.panes[&pid].tabs[ti].lang();
                                    if let Some(srv) = s.lsp.server_for_lang_host_mut(lang, path.ssh_host()) {
                                        if srv.initialized { lsp::request_formatting(srv, &path, gen_now, save_after); }
                                    }
                                }
                            }
                        } else if changed && save_after {
                            if let Some((pid, ti)) = find_tab_for_path(s, &path) { save_tab(s, pid, ti); }
                        }
                    }
                    None => {}
                }
                s.needs_redraw = true;
            }
            UserEvent::LspStopRequested { op_id, reply } => {
                // Another local-text process asked us to stop this server so it can
                // claim the slot. Kill it now and ack; the stdout-EOF LspServerStopped
                // that follows just does UI cleanup (the server is already gone).
                let result = if let Some(mut srv) = s.lsp.servers.remove(&op_id) {
                    let _ = srv.process.kill();
                    let _ = srv.process.wait();
                    lsp_registry::unregister(op_id);
                    lsp_registry::StopResult::Stopped
                } else {
                    lsp_registry::StopResult::NotFound
                };
                let _ = reply.send(result);
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::LspSlotFreed { lang, ssh_host, root } => {
                ensure_lsp_server_running(s, lang, ssh_host, root);
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RaiseWindowRequested => {
                s.win.focus_window();
                s.win.set_minimized(false);
                s.win.request_redraw();
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::LspServerStopped { server_id } => {
                lsp_registry::unregister(server_id);
                if let Some(mut srv) = s.lsp.servers.remove(&server_id) {
                    let _ = srv.process.kill(); // stdout closed: make sure it's gone, then reap
                    let _ = srv.process.wait();
                    let scope = srv.ssh_host.as_ref()
                        .map(|h| format!("remote {}", h.display()))
                        .unwrap_or_else(|| "local machine".to_owned());
                    let bin = lsp_binary(srv.lang).unwrap_or("language server");
                    let msg = if srv.initialized {
                        format!("{bin} stopped on {scope}")
                    } else {
                        format!("{bin} failed to initialize on {scope}; check LSP output")
                    };
                    s.lsp_start_errors.insert((srv.ssh_host, srv.lang), msg.clone());
                    if let Some(tp) = s.term_panes.get_mut(&server_id) {
                        let banner = format!("\x1b[31m[local-text] {msg}\x1b[0m\n");
                        terminal::feed_bytes(tp, banner.as_bytes());
                    }
                    show_lsp_output_pane(s, server_id);
                    s.status_msg = Some(msg);
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::LspBinaryCheckResult { host, statuses, error } => {
                match host {
                    Some(h) => {
                        if let Some(msg) = error {
                            s.remote_lsp_check_errors.insert(h.clone(), msg.clone());
                            for lang in [Lang::TypeScript, Lang::Rust, Lang::Python] {
                                s.remote_lsp_installed.remove(&(h.clone(), lang));
                            }
                            s.status_msg = Some(format!("LSP check failed on {}: {}", h.display(), msg));
                        } else {
                            s.remote_lsp_check_errors.remove(&h);
                        }
                        for (lang, installed) in statuses {
                            if let Some(installed) = installed {
                                s.remote_lsp_installed.insert((h.clone(), lang), installed);
                            }
                        }
                    }
                    None => {
                        for (lang, installed) in statuses {
                            if let Some(installed) = installed {
                                s.lsp_installed.insert(lang, installed);
                            }
                        }
                    }
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::FormatterDone { path, generation } => {
                // Reload the formatted file only if the buffer is exactly what was
                // saved; edits typed while the formatter ran must not be discarded.
                if let (Some((pid, ti)), Some(local)) = (find_tab_for_path(s, &path), path.as_local_path()) {
                    let limit = s.settings.undo_limit;
                    let collab = s.collab.is_some();
                    let tab = &mut s.panes.get_mut(&pid).unwrap().tabs[ti];
                    if tab.edit_generation != generation {
                        s.status_msg = Some(format!(
                            "Formatter updated {} on disk, but you kept editing — your buffer was kept (save to overwrite)",
                            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
                    } else if let Ok(text) = std::fs::read_to_string(local) {
                        if tab.text != text.as_str() {
                            let before = tab.text.clone();
                            if !collab { tab.push_undo_snapshot(limit); }
                            tab.replace_text(Rope::from_str(&text));
                            tab.dirty = false;
                            after_background_edit(s, pid, ti, Some(before));
                        }
                    }
                }
                if s.left_view == LeftView::Git && s.left_panel_visible {
                    s.git_panel.loading = true;
                    trigger_git_status_refresh(s);
                }
                s.needs_redraw = true;
            }
            UserEvent::GitOpFailed { msg } => {
                s.status_msg = Some(format!("git: {msg}"));
            }
            UserEvent::GitStatusResult { staged, unstaged, is_git_repo, error, repo_root } => {
                s.git_panel.repo_root     = repo_root;
                s.git_panel.staged        = staged;
                s.git_panel.unstaged      = unstaged;
                s.git_panel.is_git_repo   = is_git_repo;
                s.git_panel.remote_error  = error;
                s.git_panel.loading       = false;
                s.needs_redraw = true;
            }
            UserEvent::GitDiffResult { buf_id, path, lines } => {
                if let Some(d) = s.git_diff_tabs.get_mut(&buf_id) {
                    if d.path == path {
                        d.lines   = lines;
                        d.loading = false;
                    }
                }
                s.needs_redraw = true;
            }
            UserEvent::SearchDone { token, results } => {
                if token == s.global_find.search_token {
                    s.global_find.results  = results;
                    s.global_find.searching = false;
                    s.needs_redraw = true;
                    self.dirty.store(true, Ordering::Release);
                }
            }
            UserEvent::QuickFinderFiles { token, entries } => {
                if token == s.quick_finder.walk_token && !s.quick_finder.remote_directory_mode {
                    let n = entries.len();
                    s.quick_finder.entries = entries;
                    s.quick_finder.loading = false;
                    // Re-apply the current query filter now that entries are populated
                    if s.quick_finder.query.is_empty() {
                        s.quick_finder.filtered = (0..n).collect();
                    } else {
                        refilter_quick_finder(s);
                    }
                    s.needs_redraw = true;
                    self.dirty.store(true, Ordering::Release);
                }
            }
            UserEvent::TreeSearchFiles { root, entries } => {
                if let Some(ex) = s.explorer.as_mut() {
                    if ex.root.as_local_path() == Some(root.as_path()) {
                        ex.tree_search_walking = false;
                        ex.tree_search_entries = entries;
                        refilter_tree_search(ex);
                        if ex.tree_search_rewalk { spawn_tree_search_walk(ex, &s.proxy); }
                    }
                }
            }
            UserEvent::GitOpDone => {
                // Close all GitDiff tabs across all panes (they're stale after a git op)
                let mut removed_buf_ids: Vec<usize> = Vec::new();
                for pane in s.panes.values_mut() {
                    for t in pane.tabs.iter().filter(|t| t.kind == TabKind::GitDiff) {
                        removed_buf_ids.push(t.buf_id);
                    }
                    pane.tabs.retain(|t| t.kind != TabKind::GitDiff);
                    pane.active = pane.active.min(pane.tabs.len().saturating_sub(1));
                    if pane.tabs.is_empty() {
                        let new_id = s.next_buf_id; s.next_buf_id += 1;
                        pane.tabs.push(Tab::untitled(new_id));
                    }
                }
                for id in removed_buf_ids { s.git_diff_tabs.remove(&id); }
                s.git_panel.loading = true;
                trigger_git_status_refresh(s);
                s.needs_redraw = true;
            }
            UserEvent::Redraw => {
                s.needs_redraw = false;
                s.win.request_redraw();
                return;
            }

            // ── SSH remote events ─────────────────────────────────────────────
            UserEvent::SshConnecting { host } => {
                s.ssh_connections.insert(host.clone(), SshConnectionState::Connecting);
                s.status_msg = Some(format!("Connecting to {}…", host.display()));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::SshConnected { host } => {
                s.ssh_connections.insert(host.clone(), SshConnectionState::Connected);
                s.status_msg = None;
                // Now that the remote $HOME is known, make `~/…` paths absolute so
                // they match the absolute paths language servers report.
                if let Some(ex) = s.explorer.as_mut() {
                    if ex.root.ssh_host() == Some(&host) {
                        if let Some(abs) = ex.root.resolve_remote_tilde() {
                            if ex.entries.is_empty() { ex.root = abs; }
                        }
                    }
                }
                for pane in s.panes.values_mut() {
                    for tab in pane.tabs.iter_mut() {
                        if tab.path.as_ref().and_then(|p| p.ssh_host()) == Some(&host) {
                            if let Some(abs) = tab.path.as_ref().and_then(|p| p.resolve_remote_tilde()) {
                                tab.path = Some(abs);
                            }
                        }
                    }
                }
                // Trigger loading of any remote tabs that are waiting for this host.
                let pending_remote: Vec<(usize, usize, VPath)> = s.panes.iter()
                    .flat_map(|(&pid, p)| p.tabs.iter().enumerate().map(move |(tidx, t)| (pid, tidx, t)))
                    .filter_map(|(pid, tidx, t)| {
                        let path = t.path.as_ref()?;
                        if path.ssh_host() == Some(&host)
                            && t.text.len_chars() == 0
                            && t.remote_load_token.is_none()
                        {
                            Some((pid, tidx, path.clone()))
                        } else { None }
                    })
                    .collect();
                for (pid, tidx, vpath) in pending_remote {
                    begin_remote_tab_read(s, pid, tidx, vpath);
                }
                // Trigger dir listing for remote explorer root on this host.
                let remote_root_to_list = s.explorer.as_ref().and_then(|ex| {
                    if ex.root.ssh_host() == Some(&host) {
                        if let VPath::Remote { host, path } = ex.root.clone() {
                            return Some((host, path));
                        }
                    }
                    None
                });
                if let Some((remote_host, remote_path)) = remote_root_to_list {
                    if let Some(explorer) = s.explorer.as_mut() {
                        explorer.pending_remote_listings.insert(remote_path.clone());
                    }
                    ssh::ssh_list_dir(remote_host, remote_path, s.proxy.clone());
                }
                let settings_open = s.panes.values().any(|p| {
                    p.kind == PaneKind::Editor && p.tabs.iter().any(|t| t.kind == TabKind::Settings)
                });
                if settings_open && lsp_scope_host(s).as_ref() == Some(&host) {
                    check_lsp_binaries(s);
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::SshError { host, msg } => {
                s.ssh_connections.insert(host.clone(), SshConnectionState::Failed(msg.clone()));
                s.status_msg = Some(format!("SSH error ({}): {}", host.display(), msg));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteFileContent { token, path, content } => {
                // Deliver content only to the tab load request that owns this token.
                // Always clear the token so it doesn't linger; only insert content
                // if the tab is still clean (user hasn't typed anything yet).
                let mut content_dropped = false;
                let mut content_loaded = false;
                for pane in s.panes.values_mut() {
                    for tab in pane.tabs.iter_mut() {
                        if tab.path.as_ref() == Some(&path) && tab.remote_load_token == Some(token) {
                            tab.remote_load_token = None;
                            if !tab.dirty && tab.text.len_chars() == 0 {
                                tab.replace_text(ropey::Rope::from_str(&content));
                                tab.max_line_len = None;
                                tab.dirty = false;
                                content_loaded = true;
                            } else {
                                content_dropped = true;
                            }
                        }
                    }
                }
                if content_dropped {
                    s.status_msg = Some(format!(
                        "Remote load: tab was edited before {} arrived — content discarded",
                        path.display_short()
                    ));
                }
                if content_loaded {
                    notify_lsp_open(s, &path);
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteFileError { token, path, msg } => {
                let mut matched = false;
                for pane in s.panes.values_mut() {
                    for tab in pane.tabs.iter_mut() {
                        if tab.path.as_ref() == Some(&path) && tab.remote_load_token == Some(token) {
                            tab.remote_load_token = None;
                            matched = true;
                        }
                    }
                }
                if matched {
                    s.status_msg = Some(format!("Remote open error ({}): {}", path.display_short(), msg));
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteWriteDone { path, generation } => {
                // Mark clean only if nothing was typed after this save started.
                for pane in s.panes.values_mut() {
                    for tab in pane.tabs.iter_mut() {
                        if tab.path.as_ref() == Some(&path) && tab.edit_generation == generation {
                            tab.dirty = false;
                        }
                    }
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteWriteError { path, msg } => {
                s.status_msg = Some(format!("Save failed ({}): {msg}", path.display_short()));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteDirListing { path, entries } => {
                // Start LSP servers for recognised project types on first root listing.
                // Check before consuming `entries` in the explorer update below.
                if s.explorer.as_ref().map_or(false, |ex| ex.root == path) {
                    start_lsp_for_remote_project(s, &path, &entries);
                }
                if let Some(ex) = s.explorer.as_mut() {
                    ex.pending_remote_listings.remove(path.as_path());
                    if path.ssh_host() == ex.root.ssh_host() {
                        apply_remote_dir_listing(ex, path.as_path(), entries);
                    }
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteDirListingError { path, msg } => {
                if let Some(explorer) = s.explorer.as_mut() {
                    explorer.pending_remote_listings.remove(path.as_path());
                }
                s.status_msg = Some(format!(
                    "Remote listing error ({}): {msg}",
                    path.display_short(),
                ));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteFsOpDone { refresh_dir, renamed, open_path } => {
                if let Some((old_path, new_path)) = renamed {
                    update_paths_after_rename(s, &old_path, &new_path);
                }
                if let Some(path) = open_path {
                    open_or_reuse_tab(s, path);
                }

                let refresh_request = match refresh_dir.clone() {
                    VPath::Remote { host, path } => s.explorer.as_mut().and_then(|explorer| {
                        if explorer.root.ssh_host() != Some(&host) {
                            return None;
                        }
                        explorer.pending_remote_listings.remove(&path);
                        explorer.pending_remote_listings.insert(path.clone());
                        Some((host, path))
                    }),
                    VPath::Local(_) => None,
                };
                if let Some((host, path)) = refresh_request {
                    ssh::ssh_list_dir(host, path, s.proxy.clone());
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteFsOpError { path, msg } => {
                s.status_msg = Some(format!(
                    "Remote file operation error ({}): {msg}",
                    path.display_short(),
                ));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }

            // ── Collab events ─────────────────────────────────────────────────
            UserEvent::CollabMessage { from_site_id, msg } => {
                handle_collab_msg(s, from_site_id, msg);
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabConnected { session, doc } => {
                // Guest: the host's current document (if we may see it) opens in a
                // new tab — the guest's own tabs are left alone.
                s.collab = Some(*session);
                s.collab_before = None;
                s.collab_file_list = Vec::new(); // a FileList follows
                if let Some(doc) = doc { guest_load_doc(s, doc); }
                let n = s.collab.as_ref().map_or(0, |sess| sess.peers.len());
                s.status_msg = Some(format!("🔒 collab: connected ({n} peer{}) — {} lists shared files",
                    if n == 1 { "" } else { "s" }, key_label("Cmd+P")));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabDisconnected => {
                notify_collab_change(s);
                detach_collab_tabs(s, None);
                s.collab = None;
                s.collab_before = None;
                s.collab_file_list.clear();
                s.status_msg = Some("Collab session ended — shared tabs are now unsaved local buffers".to_owned());
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabError { msg } => {
                s.status_msg = Some(format!("Collab error: {msg}"));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabGuestJoined { site_id, peer } => {
                // Host: assign the default role, send Welcome (with the session's
                // document only if this role may see it), then the file list.
                notify_collab_change(s);
                let Some(sess) = s.collab.as_mut() else { return };
                let role = sess.role_perms.default_role;
                let new_peer = collab::PeerInfo { role, ..peer };
                let name = new_peer.name.clone();
                let all_peers = sess.peers.clone();
                sess.peers.push(new_peer.clone());
                let doc_path = sess.doc_path.clone();
                let rel = (!doc_path.is_empty())
                    .then(|| host_rel_path(s, &VPath::Local(PathBuf::from(&doc_path))))
                    .flatten();
                let mut doc = None;
                if let Some(rel) = rel {
                    // Same checks as `host_send_doc`, including that `rel` is the file's
                    // canonical name (not a symlink to something the guest may not see).
                    let root = s.explorer.as_ref().and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()));
                    let may_view = s.collab.as_ref().map_or(false, |sess| sess.guest_may_view(site_id, &rel))
                        && root.map_or(false, |root| collab::resolve_under_root(&root, &rel).is_some());
                    if let (true, Some((pid, ti))) = (may_view, find_collab_tab(s, &rel)) {
                        let content: String = s.panes[&pid].tabs[ti].text.chunks().collect();
                        if let Some(sess) = s.collab.as_mut() {
                            let rev = sess.server_docs.entry(rel.clone()).or_default().rev;
                            sess.open_docs.entry(site_id).or_default().insert(rel.clone());
                            doc = Some(collab::DocSnapshot { path: rel, content, rev });
                        }
                    }
                }
                if let Some(sess) = s.collab.as_ref() {
                    let welcome = collab::CollabMsg::Welcome {
                        your_site_id: site_id,
                        peers: all_peers,
                        role,
                        role_perms: sess.role_perms.clone(),
                        doc,
                    };
                    sess.send_to_site(site_id, &welcome);
                    sess.broadcast_except_msg(Some(site_id), &collab::CollabMsg::PeerJoined { peer: new_peer });
                }
                spawn_collab_file_list(s, site_id);
                s.status_msg = Some(format!("{name} joined the session"));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabFileListReady { site_id, paths } => {
                if let Some(sess) = s.collab.as_ref() {
                    if sess.peer_role(site_id).is_some() {
                        sess.send_to_site(site_id, &collab::CollabMsg::FileList { paths });
                    }
                }
            }
            UserEvent::CollabGuestLeft { site_id } => {
                let name = s.collab.as_ref()
                    .and_then(|sess| sess.peers.iter().find(|p| p.site_id == site_id))
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| format!("Guest {site_id}"));
                if let Some(session) = s.collab.as_mut() {
                    session.peers.retain(|p| p.site_id != site_id);
                    session.remote_cursors.remove(&site_id);
                    session.open_docs.remove(&site_id);
                    // Broadcast PeerLeft to remaining guests
                    session.broadcast_except_msg(None, &collab::CollabMsg::PeerLeft { site_id });
                }
                s.status_msg = Some(format!("{name} left the session"));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
        }
        if schedule_redraw {
            s.needs_redraw = true;
            self.dirty.store(true, Ordering::Release);
            if s.settings.vsync && self.display_link.is_none() {
                s.needs_redraw = false;
                s.win.request_redraw();
            }
        }
    }
}


/// Run an event handler; if it panics, save unsaved buffers to the recovery
/// folder before letting the panic continue (the app state may be inconsistent,
/// but the text buffers are what matter).
fn guard_panics(app: &mut App, f: impl FnOnce(&mut App)) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(app)));
    if let Err(payload) = result {
        if let Some(s) = app.state.as_ref() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write_recovery_files(s)));
        }
        std::panic::resume_unwind(payload);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, el: &ActiveEventLoop, cause: StartCause) {
        guard_panics(self, |app| app.handle_new_events(el, cause));
    }
    fn resumed(&mut self, el: &ActiveEventLoop) {
        self.handle_resumed(el);
    }
    fn exiting(&mut self, el: &ActiveEventLoop) {
        self.handle_exiting(el);
    }
    fn window_event(&mut self, el: &ActiveEventLoop, id: winit::window::WindowId, event: WindowEvent) {
        guard_panics(self, |app| app.handle_window_event(el, id, event));
    }
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        guard_panics(self, |app| app.handle_about_to_wait(el));
    }
    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        guard_panics(self, |app| app.handle_user_event(el, event));
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn fill(buf: &mut [u32], w: u32, h: u32, x: i32, y: i32, rw: i32, rh: i32, color: u32) {
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = (x + rw).min(w as i32) as u32;
    let y1 = (y + rh).min(h as i32) as u32;
    for py in y0..y1 {
        for px in x0..x1 {
            buf[(py * w + px) as usize] = color;
        }
    }
}

fn fill_clipped(buf: &mut [u32], w: u32, h: u32, clip_top: i32, clip_bot: i32, x: i32, y: i32, rw: i32, rh: i32, color: u32) {
    let y0 = y.max(clip_top);
    let y1 = (y + rh).min(clip_bot);
    if y1 > y0 { fill(buf, w, h, x, y0, rw, y1 - y0, color); }
}

fn blit(buf: &mut [u32], w: u32, h: u32, bmap: &[u8], m: &Metrics, x: i32, baseline: i32, fg: u32) {
    if bmap.is_empty() { return; }
    let gx = x + m.xmin;
    let gy = baseline - m.ymin as i32 - m.height as i32;
    for row in 0..m.height as i32 {
        for col in 0..m.width as i32 {
            let px = gx + col;
            let py = gy + row;
            if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 { continue; }
            let cov = bmap[(row * m.width as i32 + col) as usize] as u32;
            if cov == 0 { continue; }
            let idx = (py as u32 * w + px as u32) as usize;
            let bg  = buf[idx];
            let r = ((fg >> 16 & 0xFF) * cov + (bg >> 16 & 0xFF) * (255 - cov)) / 255;
            let g = ((fg >>  8 & 0xFF) * cov + (bg >>  8 & 0xFF) * (255 - cov)) / 255;
            let b = ((fg       & 0xFF) * cov + (bg       & 0xFF) * (255 - cov)) / 255;
            buf[idx] = (r << 16) | (g << 8) | b;
        }
    }
}

fn draw_str(buf: &mut [u32], w: u32, h: u32, g: &Glyphs, text: &str, mut x: i32, base: i32, fg: u32, clip_r: i32) {
    for ch in text.chars() {
        if x >= clip_r { break; }
        if x + g.cw > 0 {
            if let Some((m, bmap)) = g.get(ch) {
                blit(buf, w, h, bmap, m, x, base, fg);
            }
        }
        x += g.cw;
    }
}

fn fit_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars { s.to_owned() }
    else {
        let t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", t)
    }
}

fn draw_squiggle(buf: &mut [u32], w: u32, h: u32, x: i32, y: i32, width: i32, color: u32) {
    for dx in 0..width {
        let yoff: i32 = if (dx % 4) < 2 { 0 } else { 2 };
        for dy in 0..2i32 {
            let px = x + dx;
            let py = y + yoff + dy;
            if px >= 0 && px < w as i32 && py >= 0 && py < h as i32 {
                buf[(py as u32 * w + px as u32) as usize] = color;
            }
        }
    }
}

// Per-pane render snapshot
struct PaneSnap {
    id:             usize,
    rect:           Rect,
    is_active:      bool,
    is_settings_tab: bool,
    is_git_diff_tab: bool,
    git_diff_snap:  Option<(Vec<DiffLine>, usize, bool)>, // (lines, scroll, loading)
    scroll:         usize,
    hscroll:        usize,
    find_h:         i32,
    editor_h:       i32,
    cursors_snap:   Vec<(usize, usize, Option<(usize, usize)>)>,
    match_ranges:   Vec<(usize, usize)>,
    find_open:      bool,
    find_repl_open: bool,
    find_input_focused: bool,
    find_focus:     FindFocus,
    case_sensitive: bool,
    whole_word:     bool,
    find_query:     String,
    find_repl:      String,
    find_cursor_q:  usize,
    find_cursor_r:  usize,
    find_sel_q:     Option<usize>,
    find_sel_r:     Option<usize>,
    lines:          Vec<(String, usize, Vec<u32>)>,
    total:          usize,
    max_line_len:   usize,
    gutter_w:       i32,
    ln_digits:      usize,
    tab_info:       Vec<(String, bool)>,
    active_tab:     usize,
    path_name:      String,
    dirty:          bool,
    cur_line:       usize,
    cur_col:        usize,
    // Diagnostics: (line, col_start, col_end, severity, message)
    diagnostics:    Vec<(usize, usize, usize, DiagSeverity, String)>,
    // Remote collab cursors: (line, col, color) — empty when collab is inactive
    remote_cursors: Vec<(usize, usize, u32)>,
}

/// Length of the common byte prefix of two ropes. Chunks the ropes still share
/// (an edited rope reuses every untouched leaf) are skipped by pointer, so this is
/// O(number of chunks) rather than O(file size).
fn common_prefix_bytes(a: &Rope, b: &Rope) -> usize {
    let (mut ia, mut ib) = (a.chunks(), b.chunks());
    let (mut ca, mut cb): (&[u8], &[u8]) = (&[], &[]);
    let mut total = 0;
    loop {
        if ca.is_empty() { match ia.next() { Some(c) => { ca = c.as_bytes(); continue; } None => return total } }
        if cb.is_empty() { match ib.next() { Some(c) => { cb = c.as_bytes(); continue; } None => return total } }
        if ca.as_ptr() == cb.as_ptr() && ca.len() == cb.len() {
            total += ca.len();
            ca = &[];
            cb = &[];
            continue;
        }
        let n = ca.len().min(cb.len());
        let m = ca[..n].iter().zip(&cb[..n]).take_while(|(x, y)| x == y).count();
        total += m;
        if m < n { return total; }
        ca = &ca[n..];
        cb = &cb[n..];
    }
}

/// Length of the common byte suffix of two ropes, capped at `max`.
fn common_suffix_bytes(a: &Rope, b: &Rope, max: usize) -> usize {
    let mut ia = a.chunks_at_byte(a.len_bytes()).0.reversed();
    let mut ib = b.chunks_at_byte(b.len_bytes()).0.reversed();
    let (mut ca, mut cb): (&[u8], &[u8]) = (&[], &[]);
    let mut total = 0;
    while total < max {
        if ca.is_empty() { match ia.next() { Some(c) => { ca = c.as_bytes(); continue; } None => break } }
        if cb.is_empty() { match ib.next() { Some(c) => { cb = c.as_bytes(); continue; } None => break } }
        if ca.as_ptr() == cb.as_ptr() && ca.len() == cb.len() {
            total += ca.len();
            ca = &[];
            cb = &[];
            continue;
        }
        let n = ca.len().min(cb.len());
        let m = ca[ca.len() - n..].iter().rev().zip(cb[cb.len() - n..].iter().rev())
            .take_while(|(x, y)| x == y).count();
        total += m;
        if m < n { break; }
        ca = &ca[..ca.len() - n];
        cb = &cb[..cb.len() - n];
    }
    total.min(max)
}

/// First and last line (in `new`) touched by the change from `old` to `new`, or
/// None if the texts are identical.
fn changed_line_range(old: &Rope, new: &Rope) -> Option<(usize, usize)> {
    let (lo, ln) = (old.len_bytes(), new.len_bytes());
    let prefix = common_prefix_bytes(old, new);
    if prefix == lo && prefix == ln { return None; }
    let suffix = common_suffix_bytes(old, new, lo.min(ln) - prefix.min(lo.min(ln)));
    let first = new.byte_to_line(prefix.min(ln));
    let last  = new.byte_to_line((ln - suffix).max(prefix).min(ln));
    Some((first, last))
}

fn render(s: &mut State) {
    let w = s.w;
    let h = s.h;
    if w == 0 || h == 0 { return; }
    if s.panes.is_empty() { return; }
    dlog!("[render] {}x{} t={}", w, h, ts());

    // Sync shared buffers: propagate the active tab's text/path/dirty to all
    // other tabs with the same buf_id (O(1) Rope clone per sibling).
    // Skipped when the active pane/tab and text version are unchanged since last sync.
    if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Editor) {
        let ap  = s.active_pane;
        let at  = s.panes[&ap].active;
        let gen = s.panes[&ap].tabs.get(at).map_or(0, |t| t.edit_generation);
        if ap != s.last_sync_pane || at != s.last_sync_tab || gen != s.last_sync_gen {
            // Only propagate on an actual edit (or a tab switch), and invalidate the
            // siblings' highlighting so they never show stale colors.
            if at < s.panes[&ap].tabs.len() {
                propagate_to_siblings(s, ap, at);
            }
            s.last_sync_pane = ap;
            s.last_sync_tab  = at;
            s.last_sync_gen  = gen;
        }
    }

    let tab_h    = s.tab_h();
    let status_h = s.status_h();
    let lh       = s.glyphs.lh;
    let asc      = s.glyphs.asc;
    let cw       = s.glyphs.cw;
    let cursor_visible = s.cursor_visible;
    let explorer_w = s.explorer_w();
    let area     = s.pane_area();
    let layout   = layout_tree(&s.pane_tree, area);
    let active_pane_id = s.active_pane;

    // Build terminal pane snapshots
    let active_term_sel = s.term_sel.clone();
    struct TermPaneSnap {
        id:             usize,
        rect:           Rect,
        is_active:      bool,
        visible_rows:   Vec<Vec<terminal::Cell>>,
        cursor_col:     usize,
        cursor_row:     usize,
        cursor_visible: bool,
        tabs:           Vec<String>,
        active_tab:     usize,
        sel:            Option<TermSel>,
    }
    let term_snaps: Vec<TermPaneSnap> = layout.iter().filter_map(|&(pid, rect)| {
        let pane = s.panes.get(&pid)?;
        if pane.kind != PaneKind::Terminal { return None; }
        let active_tid = pane.term_ids.get(pane.active).copied()?;
        let tp = s.term_panes.get(&active_tid)?;
        let tabs: Vec<String> = pane.term_ids.iter()
            .filter_map(|&tid| s.term_panes.get(&tid).map(|t| t.title.clone()))
            .collect();
        // Hide cursor when scrolled into history
        let cur_vis = cursor_visible && tp.grid.scroll_offset == 0;
        Some(TermPaneSnap {
            id: pid,
            rect,
            is_active: pid == active_pane_id,
            visible_rows: tp.grid.visible_rows(),
            cursor_col: tp.grid.cur_col,
            cursor_row: tp.grid.cur_row,
            cursor_visible: cur_vis,
            tabs,
            active_tab: pane.active,
            sel: if pid == active_pane_id { active_term_sel.clone() } else { None },
        })
    }).collect();

    // Preload glyphs for all visible terminal characters before entering render closure
    for snap in &term_snaps {
        for row in &snap.visible_rows {
            for cell in row {
                if cell.ch > ' ' {
                    s.glyphs.load(cell.ch);
                }
            }
        }
    }

    // Build markdown preview pane snapshots (with per-pane line cache)
    struct MdPaneSnap {
        rect:        Rect,
        is_active:   bool,
        lines:       Vec<(String, Vec<u32>)>,
        title:       String,
        total_lines: usize,
        scroll:      usize,
    }
    // Cache-update pass: re-render only when source text changed.
    let md_pane_ids: Vec<usize> = layout.iter()
        .filter_map(|&(pid, _)| s.panes.get(&pid)
            .filter(|p| p.kind == PaneKind::MarkdownPreview)
            .map(|_| pid))
        .collect();
    for pid in md_pane_ids {
        let source_buf_id = match s.md_panes.get(&pid) { Some(m) => m.source_buf_id, None => continue };
        let source_gen = s.panes.values()
            .flat_map(|p| p.tabs.iter())
            .filter(|t| t.buf_id == source_buf_id)
            .map(|t| t.edit_generation)
            .max()
            .unwrap_or(0);
        if s.md_panes.get(&pid).map_or(true, |m| m.source_edit_gen != source_gen) {
            let source_text = s.panes.values()
                .find_map(|p| p.tabs.iter()
                    .find(|t| t.buf_id == source_buf_id)
                    .map(|t| t.text.clone()));
            if let (Some(mp), Some(text)) = (s.md_panes.get_mut(&pid), source_text) {
                mp.lines_cache = render_markdown_to_lines(&text);
                mp.source_edit_gen = source_gen;
            }
        }
    }
    let md_snaps: Vec<MdPaneSnap> = layout.iter().filter_map(|&(pid, rect)| {
        let pane = s.panes.get(&pid)?;
        if pane.kind != PaneKind::MarkdownPreview { return None; }
        let mp = s.md_panes.get(&pid)?;
        let total_lines = mp.lines_cache.len();
        let content_h = (rect.h - tab_h).max(0);
        let vis = (content_h / lh).max(1) as usize;
        let lines = mp.lines_cache.iter().skip(mp.scroll).take(vis).cloned().collect();
        Some(MdPaneSnap {
            rect, is_active: pid == active_pane_id,
            lines, title: mp.title.clone(), total_lines, scroll: mp.scroll,
        })
    }).collect();

    let rainbow = s.settings.rainbow_brackets;

    // Pre-pass: update hl_cache and max_line_len for all visible editor panes (mutable pass)
    for &(pid, rect) in &layout {
        let Some(pane) = s.panes.get_mut(&pid) else { continue };
        if pane.kind != PaneKind::Editor { continue; }
        let active = pane.active;
        let Some(tab) = pane.tabs.get_mut(active) else { continue };
        if tab.kind == TabKind::Settings || tab.kind == TabKind::GitDiff { continue; }
        let lang = tab.lang();
        let fh  = if pane.find.open { lh + 4 + if pane.find.replace_open { lh + 4 } else { 0 } } else { 0 };
        let vis = ((rect.h - tab_h - fh).max(0) / lh).max(1) as usize;
        let scroll = tab.scroll;
        let total  = tab.text.len_lines();

        // Fold edits since the last frame into the caches: only lines from the first
        // changed one need re-highlighting, and only changed lines can raise the max
        // line length (it is kept as an upper bound rather than rescanning the file).
        if let Some(before) = tab.hl_before.take() {
            if let Some((first, last)) = changed_line_range(&before, &tab.text) {
                tab.hl_dirty_from = tab.hl_dirty_from.min(first);
                tab.hl_color_cache.retain(|&li, _| li < first);
                if let Some(old_max) = tab.max_line_len {
                    let changed_max = (first..=last.min(total.saturating_sub(1)))
                        .map(|li| State::line_len(&tab.text, li)).max().unwrap_or(0);
                    tab.max_line_len = Some(old_max.max(changed_max));
                }
            }
        }
        // Full scan only when unknown (file load).
        if tab.max_line_len.is_none() {
            tab.max_line_len = Some((0..total).map(|li| State::line_len(&tab.text, li)).max().unwrap_or(0));
        }

        // Rebuild hl_cache from hl_dirty_from up through scroll + vis.
        // hl_cache[n] = tokenizer state at the *start* of line n, for every
        // n <= hl_dirty_from (including the first line not yet computed).
        if lang != Lang::None {
            let need_up_to = (scroll + vis).min(total);
            tab.hl_cache.resize(total + 1, (MlState::Normal, 0));
            tab.hl_dirty_from = tab.hl_dirty_from.min(total);
            if tab.hl_dirty_from < need_up_to {
                let (mut state, mut depth) = if tab.hl_dirty_from == 0 {
                    (MlState::Normal, 0i32)
                } else {
                    tab.hl_cache[tab.hl_dirty_from]
                };
                for li in tab.hl_dirty_from..need_up_to {
                    tab.hl_cache[li] = (state, depth);
                    let chars: Vec<char> = tab.text.line(li)
                        .chars().take_while(|&c| c != '\n' && c != '\r').collect();
                    let (colors, ns, nd) = highlight_line(&chars, lang, state, rainbow, depth);
                    state = ns;
                    depth = nd;
                    if li >= scroll { tab.hl_color_cache.insert(li, colors); }
                }
                tab.hl_cache[need_up_to] = (state, depth);
                tab.hl_dirty_from = need_up_to;
            }
            // Only the visible lines keep their colors (RAM stays proportional to
            // the window, not the file).
            tab.hl_color_cache.retain(|&li, _| li >= scroll && li < scroll + vis);
        }

        // Refresh find-match cache if the query, flags, or file content changed.
        // Doing this in the mutable pre-pass means the snapshot pass just clones it.
        if pane.find.open && !pane.find.query.is_empty() {
            let key = (pane.find.query.clone(), pane.find.case_sensitive, pane.find.whole_word);
            let gen = tab.edit_generation;
            if pane.find.match_cache_gen != gen || pane.find.match_cache_key != key {
                pane.find.match_cache = find_matches(&tab.text, &key.0, key.1, key.2);
                pane.find.match_cache_gen = gen;
                pane.find.match_cache_key = key;
            }
        }
    }

    // Build per-pane snapshots (editor panes only)
    let pane_snaps: Vec<PaneSnap> = layout.iter().filter_map(|&(pid, rect)| {
        let pane = s.panes.get(&pid)?;
        if pane.kind != PaneKind::Editor { return None; }

        // Helper: build tab_info with ∆ suffix for GitDiff tabs
        let make_tab_info = |tabs: &[Tab]| -> Vec<(String, bool)> {
            tabs.iter().map(|t| {
                if t.kind == TabKind::GitDiff {
                    let name = t.path.as_ref()
                        .and_then(|p| p.file_name()).and_then(|n| n.to_str())
                        .unwrap_or("Diff");
                    (format!("{} \u{0394}", name), false)
                } else {
                    (t.display_name().to_owned(), t.dirty)
                }
            }).collect()
        };

        // GitDiff tab: build a minimal snap
        if pane.tabs.get(pane.active).map_or(false, |t| t.kind == TabKind::GitDiff) {
            let tab = &pane.tabs[pane.active];
            let buf_id = tab.buf_id;
            let tab_scroll = tab.scroll;
            let gd = s.git_diff_tabs.get(&buf_id);
            let git_diff_snap = gd.map(|d| (d.lines.clone(), tab_scroll, d.loading));
            let tab_info = make_tab_info(&pane.tabs);
            return Some(PaneSnap {
                id: pid, rect, is_active: pid == active_pane_id,
                is_settings_tab: false, is_git_diff_tab: true,
                git_diff_snap,
                tab_info, active_tab: pane.active,
                scroll: tab_scroll, hscroll: 0, find_h: 0, editor_h: 0,
                cursors_snap: vec![], match_ranges: vec![],
                find_open: false, find_repl_open: false, find_input_focused: false,
                find_focus: FindFocus::Query, case_sensitive: false, whole_word: false,
                find_query: String::new(), find_repl: String::new(),
                find_cursor_q: 0, find_cursor_r: 0,
                find_sel_q: None, find_sel_r: None,
                lines: vec![], total: 0, max_line_len: 0,
                gutter_w: 0, ln_digits: 0,
                path_name: String::new(), dirty: false,
                cur_line: 0, cur_col: 0, diagnostics: vec![], remote_cursors: vec![],
            });
        }

        // Settings tab: build a minimal snap (no lines/cursors needed)
        if pane.tabs.get(pane.active).map_or(false, |t| t.kind == TabKind::Settings) {
            let tab_info = make_tab_info(&pane.tabs);
            let settings_scroll = pane.tabs.get(pane.active).map_or(0, |t| t.scroll);
            return Some(PaneSnap {
                id: pid, rect, is_active: pid == active_pane_id,
                is_settings_tab: true, is_git_diff_tab: false, git_diff_snap: None,
                tab_info, active_tab: pane.active,
                scroll: settings_scroll, hscroll: 0, find_h: 0, editor_h: 0,
                cursors_snap: vec![], match_ranges: vec![],
                find_open: false, find_repl_open: false, find_input_focused: false,
                find_focus: FindFocus::Query, case_sensitive: false, whole_word: false,
                find_query: String::new(), find_repl: String::new(),
                find_cursor_q: 0, find_cursor_r: 0,
                find_sel_q: None, find_sel_r: None,
                lines: vec![], total: 0, max_line_len: 0,
                gutter_w: 0, ln_digits: 0,
                path_name: String::from("Settings"), dirty: false,
                cur_line: 0, cur_col: 0, diagnostics: vec![], remote_cursors: vec![],
            });
        }

        let tab  = pane.tab();
        let fh   = State::pane_find_h(pane, lh);
        let eh   = (rect.h - tab_h - fh).max(0);
        let vis  = (eh / lh).max(1) as usize;
        let scroll  = tab.scroll;
        let hscroll = tab.hscroll;
        let total   = tab.text.len_lines();
        let lang    = tab.lang();

        let cursors_snap: Vec<(usize, usize, Option<(usize, usize)>)> = tab.cursors.iter().map(|c| {
            let head = c.head.min(tab.text.len_chars());
            let line = tab.text.char_to_line(head);
            let col  = head - tab.text.line_to_char(line);
            (line, col, c.sel())
        }).collect();
        let (cur_line, cur_col) = cursors_snap.last().map(|&(l, c, _)| (l, c)).unwrap_or((0, 0));

        let fq = pane.find.query.clone();
        let match_ranges: Vec<(usize, usize)> = if pane.find.open && !fq.is_empty() {
            pane.find.match_cache.clone()
        } else { vec![] };

        // Syntax highlight lines — use hl_cache for pre-scroll state (populated in pre-pass above)
        let (mut hl_state, mut bracket_depth) = if lang != Lang::None {
            tab.hl_cache.get(scroll).copied().unwrap_or((MlState::Normal, 0))
        } else {
            (MlState::Normal, 0i32)
        };
        let line_count = vis.min(total.saturating_sub(scroll));
        let mut lines: Vec<(String, usize, Vec<u32>)> = Vec::with_capacity(line_count);
        let mut char_buf: Vec<char> = Vec::with_capacity(256);
        for vi in 0..line_count {
            let li         = scroll + vi;
            let line_start = tab.text.line_to_char(li);
            char_buf.clear();
            char_buf.extend(tab.text.line(li).chars().take_while(|&c| c != '\n' && c != '\r'));
            let text: String = char_buf.iter().collect();
            let colors = if let Some(cached) = tab.hl_color_cache.get(&li).filter(|v| !v.is_empty()) {
                if let Some(&(ns, nd)) = tab.hl_cache.get(li + 1) {
                    hl_state = ns;
                    bracket_depth = nd;
                }
                cached.clone()
            } else if lang != Lang::None {
                let (c, ns, bd) = highlight_line(&char_buf, lang, hl_state, rainbow, bracket_depth);
                hl_state = ns;
                bracket_depth = bd;
                c
            } else {
                vec![FG; char_buf.len()]
            };
            lines.push((text, line_start, colors));
        }

        let max_line_len = tab.max_line_len.unwrap_or(0);
        let ln_digits = State::line_num_digits(total);
        let gutter_w  = State::gutter_w(total, cw);
        let tab_info = make_tab_info(&pane.tabs);

        // Remote collab cursors — convert char offsets to (line, col, color).
        // Only show cursors for peers whose current_file matches this tab's path.
        let tab_rel = tab_collab_rel(s, tab);
        let remote_cursors: Vec<(usize, usize, u32)> = s.collab.as_ref().map_or(vec![], |session| {
            session.remote_cursors.iter().flat_map(|(&sid, (rc_path, rc_vec))| {
                // Only show a peer's cursor on the tab for the same shared document.
                if tab_rel.as_deref() != Some(rc_path.as_str()) {
                    return vec![];
                }
                let color = session.peers.iter()
                    .find(|p| p.site_id == sid)
                    .map_or(0xFF6B6B, |p| p.color);
                rc_vec.iter().map(move |rc| {
                    let head  = rc.head.min(tab.text.len_chars());
                    let line  = tab.text.char_to_line(head);
                    let col   = head - tab.text.line_to_char(line);
                    (line, col, color)
                }).collect::<Vec<_>>()
            }).collect()
        });

        Some(PaneSnap {
            id: pid,
            rect,
            is_active: pid == active_pane_id,
            is_settings_tab: false, is_git_diff_tab: false, git_diff_snap: None,
            scroll, hscroll, find_h: fh, editor_h: eh,
            cursors_snap, match_ranges,
            find_open: pane.find.open, find_repl_open: pane.find.replace_open,
            find_input_focused: pane.find.input_focused,
            find_focus: pane.find.focus, case_sensitive: pane.find.case_sensitive,
            whole_word: pane.find.whole_word,
            find_query: fq, find_repl: pane.find.replace.clone(),
            find_cursor_q: pane.find.query[..clamp_byte_index(&pane.find.query, pane.find.cursor_query)].chars().count(),
            find_cursor_r: pane.find.replace[..clamp_byte_index(&pane.find.replace, pane.find.cursor_replace)].chars().count(),
            find_sel_q: pane.find.sel_anchor_q.map(|a| pane.find.query[..clamp_byte_index(&pane.find.query, a)].chars().count()),
            find_sel_r: pane.find.sel_anchor_r.map(|a| pane.find.replace[..clamp_byte_index(&pane.find.replace, a)].chars().count()),
            lines, total, max_line_len, gutter_w, ln_digits,
            tab_info, active_tab: pane.active,
            path_name: tab.display_name().to_owned(), dirty: tab.dirty,
            cur_line, cur_col,
            diagnostics: tab.path.as_ref()
                .and_then(|p| s.diagnostics.get(p))
                .map(|diags| diags.iter().map(|d| (d.line, d.col_start, d.col_end, d.severity.clone(), d.message.clone())).collect())
                .unwrap_or_default(),
            remote_cursors,
        })
    }).collect();

    // Drag snapshot
    let drag_snap: Option<(usize, Option<usize>, Option<DropZone>)> =
        s.drag.as_ref().map(|d| (d.source_pane, d.over_pane, d.zone));
    let drag_src_is_terminal = s.drag.as_ref()
        .and_then(|d| s.panes.get(&d.source_pane))
        .map_or(false, |p| p.kind == PaneKind::Terminal);

    let show_hidden = s.explorer.as_ref().map_or(false, |ex| ex.show_hidden);
    let tree_scroll_snap = s.explorer.as_ref().map_or(0, |ex| {
        // Clamp in case entries shrank (collapse/search) since last scroll update
        ex.tree_scroll.min(ex.entries.len().saturating_sub(1))
    });
    let explorer_snap: Option<Vec<(String, bool, bool, usize, bool)>> =
        s.explorer.as_ref().map(|ex| {
            ex.entries.iter().enumerate().map(|(i, e)| {
                (e.name.clone(), e.is_dir, e.expanded, e.depth, i == ex.selected)
            }).collect()
        });

    let renderer_is_gpu  = s.renderer.is_gpu();
    let vsync_on         = s.settings.vsync;
    let rainbow_brackets = s.settings.rainbow_brackets;
    let undo_limit       = s.settings.undo_limit;
    let term_copy_paste  = s.settings.term_copy_paste;
    let term_cmd_bs      = s.settings.term_cmd_bs;
    let term_alt_bs      = s.settings.term_alt_bs;
    let term_word_select = s.settings.term_word_select;
    let lsp_scope_host   = lsp_scope_host(s);
    let lsp_scope_label  = lsp_scope_host.as_ref()
        .map(|h| format!("SSH {}", h.display()))
        .unwrap_or_else(|| "local".to_owned());
    let lsp_rows_snap: Vec<(Lang, &'static str, String, bool, bool)> =
        [(Lang::TypeScript, "TypeScript"), (Lang::Rust, "Rust      "), (Lang::Python, "Python    ")]
            .into_iter()
            .map(|(lang, name)| {
                let srv = s.lsp.server_for_lang_host(lang, lsp_scope_host.as_ref());
                let installed = lsp_installed_for(s, lang, lsp_scope_host.as_ref());
                let failed = s.lsp_start_errors.contains_key(&(lsp_scope_host.clone(), lang));
                let check_failed = lsp_scope_host.as_ref()
                    .and_then(|host| s.remote_lsp_check_errors.get(host))
                    .is_some();
                let connection_failed = lsp_scope_host.as_ref()
                    .and_then(|host| s.ssh_connections.get(host))
                    .map_or(false, |state| matches!(state, SshConnectionState::Failed(_)));
                let (status, accent, show_install) = if let Some(srv) = srv {
                    if srv.initialized {
                        ("running".to_owned(), true, false)
                    } else {
                        ("starting".to_owned(), false, false)
                    }
                } else if failed {
                    if installed == Some(false) {
                        ("not found".to_owned(), false, true)
                    } else {
                        ("failed".to_owned(), false, false)
                    }
                } else {
                    match installed {
                        Some(true)  => ("installed".to_owned(), false, false),
                        Some(false) => ("not found".to_owned(), false, true),
                        None if check_failed || connection_failed => ("check failed".to_owned(), false, false),
                        None        => ("checking".to_owned(), false, false),
                    }
                };
                (lang, name, status, accent, show_install)
            })
            .collect();
    let format_on_save          = s.settings.format_on_save.clone();
    let organize_imports_on_save = s.settings.organize_imports_on_save.clone();
    let format_command          = s.settings.format_command.clone();
    let glyph_cache_limit       = s.settings.glyph_cache_limit;
    let cpu_double_buffer       = s.settings.cpu_double_buffer;
    let gpu_drawable_count      = s.settings.gpu_drawable_count;
    let settings_edit_field     = s.settings_edit_field;
    let settings_edit_text      = s.settings_edit_text.clone();
    let settings_edit_cursor    = s.settings_edit_cursor;
    let collab_port             = s.settings.collab_port;
    let collab_role_perms       = s.settings.collab_role_perms.clone();
    let collab_include_globs    = s.settings.collab_include_globs.join(",");
    let collab_exclude_globs    = s.settings.collab_exclude_globs.join(",");
    // For collab panel: snapshot of live session peers
    let collab_peers_snap: Vec<(u64, String, u32, collab::PeerRole, String)> = s.collab.as_ref()
        .map(|sess| sess.peers.iter().map(|p| {
            (p.site_id, p.name.clone(), p.color, p.role, p.current_file.clone())
        }).collect())
        .unwrap_or_default();
    let collab_session_active   = s.collab.is_some();
    let collab_is_host          = s.collab.as_ref()
        .map_or(false, |sess| matches!(sess.role, collab::CollabRole::Host { .. }));
    let collab_invite_str: Option<String> = s.collab.as_ref().and_then(|sess| {
        if let collab::CollabRole::Host { invite_str, .. } = &sess.role {
            Some(invite_str.clone())
        } else { None }
    });
    let explorer_drag    = s.explorer_drag;
    let ui_scale         = s.font_size / FONT_PX;
    let left_view        = s.left_view;
    let left_panel_visible = s.left_panel_visible;
    let sidebar_list_focused = s.sidebar_list_focused;
    let act_w            = s.activity_bar_w();
    let status_msg       = s.status_msg.clone();
    // Collab status shown in right side of status bar when active and no other status_msg
    let collab_status: Option<String> = s.collab.as_ref().map(|sess| {
        let n = sess.peer_count();
        let ps = if n == 1 { "" } else { "s" };
        match &sess.role {
            collab::CollabRole::Host { invite_str, .. } => {
                format!(" 🔒 hosting ({n} peer{ps}) | {invite_str} ")
            }
            collab::CollabRole::Guest { .. } => {
                format!(" 🔒 collab ({n} peer{ps}) ")
            }
        }
    });

    // Quick finder / command palette snapshot (unified)
    let qf_open         = s.quick_finder.open;
    let qf_loading      = s.quick_finder.loading;
    let qf_query        = s.quick_finder.query.clone();
    let qf_cursor_chars = s.quick_finder.query[..s.quick_finder.cursor].chars().count();
    let qf_sel_anchor_chars: Option<usize> = s.quick_finder.sel_anchor.map(|a|
        s.quick_finder.query[..a].chars().count());
    let qf_is_cmd_mode  = qf_query.starts_with('>');
    let qf_is_goto_mode = qf_query.starts_with(':');
    let qf_remote_directory_mode = s.quick_finder.remote_directory_mode;
    let qf_selected     = s.quick_finder.selected;
    let qf_items: Vec<(String, String)> = if qf_open && !qf_is_cmd_mode && !qf_is_goto_mode {
        let n = s.quick_finder.filtered.len();
        let view_start = qf_selected.saturating_sub(4).min(n.saturating_sub(10));
        let view_end   = (view_start + 10).min(n);
        s.quick_finder.filtered[view_start..view_end].iter().map(|&idx| {
            let p = &s.quick_finder.entries[idx];
            let (name, dir) = if qf_remote_directory_mode {
                (remote_uri(p).unwrap_or_else(|| p.display_short()), String::new())
            } else {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
                let dir  = p.parent_str();
                (name, dir)
            };
            (name, dir)
        }).collect()
    } else { vec![] };
    let qf_sel_in_view = if qf_open && !qf_is_cmd_mode && !qf_is_goto_mode {
        let n = s.quick_finder.filtered.len();
        let view_start = qf_selected.saturating_sub(4).min(n.saturating_sub(10));
        qf_selected.saturating_sub(view_start)
    } else { 0 };
    // Direct path entry (for absolute/~ queries with no fuzzy results)
    let qf_direct_item: Option<(String, String)> = if qf_open && !qf_is_cmd_mode && !qf_is_goto_mode
        && qf_items.is_empty() && !qf_loading
    {
        s.quick_finder.direct_path.as_ref().map(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
            let dir  = p.parent_str();
            (name, dir)
        })
    } else { None };
    let qf_cmd_items: Vec<(String, String)> = if qf_open && qf_is_cmd_mode {
        let n = s.quick_finder.filtered_commands.len();
        let view_start = qf_selected.saturating_sub(4).min(n.saturating_sub(10));
        let view_end   = (view_start + 10).min(n);
        s.quick_finder.filtered_commands[view_start..view_end].iter().map(|cmd| {
            match cmd {
                QfCommand::Static(i) => (COMMANDS[*i].name.to_owned(), key_label(COMMANDS[*i].shortcut).into_owned()),
                QfCommand::LspOutput { title, .. } => (format!("Open LSP Output: {}", title), String::new()),
            }
        }).collect()
    } else { vec![] };
    let qf_cmd_sel_in_view = if qf_open && qf_is_cmd_mode {
        let n = s.quick_finder.filtered_commands.len();
        let view_start = qf_selected.saturating_sub(4).min(n.saturating_sub(10));
        qf_selected.saturating_sub(view_start)
    } else { 0 };

    // Tree search snapshot
    let tree_search_snap: Option<(String, bool, bool, Vec<(String, String)>, usize, usize, Option<usize>)> =
        s.explorer.as_ref().map(|ex| {
            let items: Vec<(String, String)> = ex.tree_search_results.iter().take(100).map(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
                let dir  = p.parent().and_then(|d| d.to_str()).unwrap_or("").to_owned();
                (name, dir)
            }).collect();
            let cursor_chars = ex.tree_search[..ex.tree_search_cursor].chars().count();
            let sel_anchor_chars = ex.tree_search_sel_anchor.map(|a| ex.tree_search[..a].chars().count());
            (ex.tree_search.clone(), ex.tree_search_focused, ex.tree_search_fuzzy, items, ex.tree_search_sel, cursor_chars, sel_anchor_chars)
        });

    // Global find snapshot
    struct GlobalFindSnap {
        query:    String, replace: String,
        include:  String, exclude: String,
        focus:    GlobalFindFocus,
        case_sensitive: bool,
        live_search: bool,
        searching: bool,
        results:  Vec<(String, usize, String, usize, usize)>,
        selected: usize, scroll: usize,
        cursor_query:   usize, cursor_replace: usize,
        cursor_include: usize, cursor_exclude: usize,
        sel_q:   Option<usize>, sel_r:   Option<usize>,
        sel_inc: Option<usize>, sel_exc: Option<usize>,
    }
    let gf_snap = if s.explorer.is_some() && left_view == LeftView::GlobalSearch {
        let gf = &s.global_find;
        Some(GlobalFindSnap {
            query:   gf.query.clone(),
            replace: gf.replace.clone(),
            include: gf.include_glob.clone(),
            exclude: gf.exclude_glob.clone(),
            focus:   gf.focus,
            case_sensitive: gf.case_sensitive,
            live_search: gf.live_search,
            searching: gf.searching,
            results: gf.results.iter().map(|r| {
                let name = r.path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
                (name, r.line_num, r.line_text.clone(), r.match_col, r.match_len)
            }).collect(),
            selected: gf.selected,
            scroll:   gf.scroll,
            cursor_query:   gf.query[..gf.cursor_query].chars().count(),
            cursor_replace: gf.replace[..gf.cursor_replace].chars().count(),
            cursor_include: gf.include_glob[..gf.cursor_include].chars().count(),
            cursor_exclude: gf.exclude_glob[..gf.cursor_exclude].chars().count(),
            sel_q:   gf.sel_anchor_q.map(|a| gf.query[..a].chars().count()),
            sel_r:   gf.sel_anchor_r.map(|a| gf.replace[..a].chars().count()),
            sel_inc: gf.sel_anchor_inc.map(|a| gf.include_glob[..a].chars().count()),
            sel_exc: gf.sel_anchor_exc.map(|a| gf.exclude_glob[..a].chars().count()),
        })
    } else { None };

    // Diagnostics panel snapshot: (filename, line, severity, message) sorted errors-first
    let diag_panel_sel  = s.diag_panel_sel;
    let diag_panel_snap: Option<Vec<(String, usize, DiagSeverity, String, VPath)>> =
        if s.explorer.is_some() && left_view == LeftView::Diagnostics {
            let mut items: Vec<(String, usize, DiagSeverity, String, VPath)> = s.diagnostics.iter()
                .flat_map(|(path, diags)| {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
                    diags.iter().map(move |d| (name.clone(), d.line, d.severity.clone(), d.message.clone(), path.clone()))
                })
                .collect();
            // Sort: errors first, then warnings, then info/hint, then by filename+line
            items.sort_by(|a, b| {
                let sev_ord = |s: &DiagSeverity| match s { DiagSeverity::Error => 0, DiagSeverity::Warning => 1, _ => 2 };
                sev_ord(&a.2).cmp(&sev_ord(&b.2)).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1))
            });
            Some(items)
        } else { None };

    // Git panel snapshot
    struct GitSnap {
        staged:         Vec<GitEntry>,
        unstaged:       Vec<GitEntry>,
        commit_msg:     String,
        commit_cursor:  usize,
        commit_focused: bool,
        is_git_repo:    bool,
        remote_error:   Option<String>,
        sel:            GitSel,
        loading:        bool,
        scroll:         usize,
    }
    let git_snap: Option<GitSnap> = if s.explorer.is_some() && left_view == LeftView::Git && left_panel_visible {
        let gp = &s.git_panel;
        Some(GitSnap {
            staged:         gp.staged.clone(),
            unstaged:       gp.unstaged.clone(),
            commit_msg:     gp.commit_msg.clone(),
            commit_cursor:  gp.commit_cursor,
            commit_focused: gp.commit_focused,
            is_git_repo:    gp.is_git_repo,
            remote_error:   gp.remote_error.clone(),
            sel:            gp.sel.clone(),
            loading:        gp.loading,
            scroll:         gp.scroll,
        })
    } else { None };

    // Error count for activity bar badge
    let err_count: usize = s.diagnostics.values()
        .map(|v| v.iter().filter(|d| d.severity == DiagSeverity::Error).count())
        .sum();

    // Context menu snapshot
    let ctx_menu_snap: Option<(i32, i32, Vec<ContextMenuItem>, usize)> =
        s.context_menu.as_ref().map(|m| (m.x, m.y, m.items.clone(), m.hovered));

    // Hover tooltip: find if mouse is over a diagnostic squiggle in any editor pane
    let mx_hover = s.mouse_x as i32;
    let my_hover = s.mouse_y as i32;
    let hover_tip: Option<(i32, i32, String)> = pane_snaps.iter().find_map(|snap| {
        let ed_x = snap.rect.x + snap.gutter_w;
        snap.diagnostics.iter().find_map(|(dline, cs, ce, _, msg)| {
            let row_y = snap.rect.y + tab_h + (*dline as i32 - snap.scroll as i32) * lh;
            if my_hover < row_y || my_hover >= row_y + lh { return None; }
            let x1 = ed_x + (*cs as i32 - snap.hscroll as i32) * cw;
            let x2 = ed_x + (*ce as i32 - snap.hscroll as i32) * cw;
            if mx_hover >= x1 && mx_hover < x2.max(x1 + cw) {
                Some((mx_hover, row_y, msg.clone()))
            } else { None }
        })
    });

    // Cmd+hover underline: when Cmd is held and the mouse is over an identifier in an editor
    // pane that has an active LSP server, underline the token to indicate goto-def is available.
    let cmd_held = primary_mod(s.mods); // Cmd (macOS) / Ctrl (Linux) + hover
    let cmd_hover_underline: Option<(i32, i32, i32, i32)> = if cmd_held {
        pane_snaps.iter().find_map(|snap| {
            if snap.is_settings_tab { return None; }
            let ed_x    = snap.rect.x + snap.gutter_w;
            let clip_r  = snap.rect.x + snap.rect.w;
            let content_y = snap.rect.y + tab_h;
            let fh = snap.find_h;
            if mx_hover < ed_x || my_hover < content_y || my_hover >= snap.rect.y + snap.rect.h - fh { return None; }
            let vi = (my_hover - content_y) / lh;
            let li = snap.scroll as i32 + vi;
            if li < 0 || li as usize >= snap.total { return None; }
            let col_raw = ((mx_hover - ed_x) / cw + snap.hscroll as i32).max(0) as usize;
            let (text, _, _) = snap.lines.get(vi as usize)?;
            let chars: Vec<char> = text.chars().collect();
            if col_raw >= chars.len() { return None; }
            let ch = chars[col_raw];
            let is_word = |c: char| c.is_alphanumeric() || c == '_';
            if !is_word(ch) { return None; }
            let mut lo = col_raw;
            while lo > 0 && is_word(chars[lo - 1]) { lo -= 1; }
            let mut hi = col_raw + 1;
            while hi < chars.len() && is_word(chars[hi]) { hi += 1; }
            let x1 = (ed_x + (lo as i32 - snap.hscroll as i32) * cw).max(ed_x);
            let x2 = (ed_x + (hi as i32 - snap.hscroll as i32) * cw).min(clip_r);
            let py_top = content_y + vi * lh;
            if x2 <= x1 { return None; }
            Some((x1, py_top, x2, py_top + lh))
        })
    } else { None };

    // LSP manager panel snapshot: (rows, selected). Built before the move closure.
    let lsp_panel_snap: Option<(Vec<String>, usize)> = s.lsp_panel.as_ref().map(|p| {
        let self_pid = std::process::id();
        let rows: Vec<String> = p.entries.iter().map(|e| {
            let scope = e.ssh_host.clone().unwrap_or_else(|| "local".to_owned());
            let root  = e.root.clone().unwrap_or_else(|| "(no root)".to_owned());
            let mine  = if e.pid == self_pid { "  (this window)" } else { "" };
            format!("{} · {} · {} · pid {}{}", e.lang, root, scope, e.pid, mine)
        }).collect();
        (rows, p.selected)
    });

    // SAFETY: render_frame takes FnOnce and calls it synchronously before returning.
    // s.glyphs is alive for the entire duration, and renderer does not alias glyphs.
    let glyphs = &s.glyphs as *const Glyphs;

    s.renderer.render_frame(move |buf, w, h| {
        // SAFETY: glyphs points into s.glyphs which outlives this synchronous closure; no mutation occurs during the frame.
        let g = unsafe { &*glyphs };

        for p in buf.iter_mut() { *p = BG; }

        // ── Activity bar + left panel ─────────────────────────────────────
        if let Some(entries) = &explorer_snap {
            let panel_h = h as i32 - status_h;

            // Activity bar (narrow strip at x=0)
            fill(buf, w, h, 0, 0, act_w, panel_h, BG);
            fill(buf, w, h, act_w - 1, 0, 1, panel_h, BORDER);

            let icon_size = lh;
            let file_icon_y = 8;
            let srch_icon_y = file_icon_y + icon_size + 4;
            let diag_icon_y = srch_icon_y + icon_size + 4;
            let gear_y      = panel_h - icon_size - 8;

            let git_icon_y    = diag_icon_y + icon_size + 4;
            let collab_icon_y = git_icon_y + icon_size + 4;

            // File tree icon
            let ft_active = left_view == LeftView::FileTree && left_panel_visible;
            if ft_active { fill(buf, w, h, 0, file_icon_y, 2, icon_size, ACCENT); }
            let ft_bg = if ft_active { SEL_BG } else { BG };
            fill(buf, w, h, 2, file_icon_y, act_w - 3, icon_size, ft_bg);
            draw_str(buf, w, h, g, " [F]", 2, file_icon_y + asc, if ft_active { ACCENT } else { FG_DIM }, act_w - 1);

            // Search icon
            let gs_active = left_view == LeftView::GlobalSearch && left_panel_visible;
            if gs_active { fill(buf, w, h, 0, srch_icon_y, 2, icon_size, ACCENT); }
            let gs_bg = if gs_active { SEL_BG } else { BG };
            fill(buf, w, h, 2, srch_icon_y, act_w - 3, icon_size, gs_bg);
            draw_str(buf, w, h, g, " [S]", 2, srch_icon_y + asc, if gs_active { ACCENT } else { FG_DIM }, act_w - 1);

            // Diagnostics icon
            let dg_active = left_view == LeftView::Diagnostics && left_panel_visible;
            if dg_active { fill(buf, w, h, 0, diag_icon_y, 2, icon_size, ACCENT); }
            let dg_bg = if dg_active { SEL_BG } else { BG };
            fill(buf, w, h, 2, diag_icon_y, act_w - 3, icon_size, dg_bg);
            let dg_label = if err_count > 0 {
                format!("[!{}]", err_count.min(99))
            } else {
                "[!]".to_owned()
            };
            let dg_color = if err_count > 0 { 0xFF5555u32 } else if dg_active { ACCENT } else { FG_DIM };
            draw_str(buf, w, h, g, &dg_label, 2, diag_icon_y + asc, dg_color, act_w - 1);

            // Git icon
            let gt_active = left_view == LeftView::Git && left_panel_visible;
            if gt_active { fill(buf, w, h, 0, git_icon_y, 2, icon_size, ACCENT); }
            let gt_bg = if gt_active { SEL_BG } else { BG };
            fill(buf, w, h, 2, git_icon_y, act_w - 3, icon_size, gt_bg);
            draw_str(buf, w, h, g, " [G]", 2, git_icon_y + asc, if gt_active { ACCENT } else { FG_DIM }, act_w - 1);

            // Collab icon — only visible when a session is active
            if collab_session_active {
                let co_active = left_view == LeftView::Collab && left_panel_visible;
                if co_active { fill(buf, w, h, 0, collab_icon_y, 2, icon_size, ACCENT); }
                let co_bg = if co_active { SEL_BG } else { BG };
                fill(buf, w, h, 2, collab_icon_y, act_w - 3, icon_size, co_bg);
                let co_color = if co_active { ACCENT } else { 0x55_AA_FF_u32 };
                draw_str(buf, w, h, g, " [C]", 2, collab_icon_y + asc, co_color, act_w - 1);
            }

            // Gear icon — centered, full-width
            fill(buf, w, h, 2, gear_y, act_w - 3, icon_size, SEL_BG);
            let gear_x = 2 + (act_w - 3 - cw) / 2;
            draw_str(buf, w, h, g, "⚙", gear_x, gear_y + asc, FG, act_w - 1);

            if left_panel_visible {
            // Left panel area (file tree, global search, diagnostics, or git)
            let px = act_w; // panel x
            let pw = explorer_w; // panel width
            fill(buf, w, h, px, 0, pw, panel_h, BG2);
            let border_col = if explorer_drag { ACCENT } else { BORDER };
            fill(buf, w, h, px + pw - 1, 0, 1, panel_h, border_col);

            if left_view == LeftView::FileTree {
                // Row 0: hidden-files toggle
                let toggle_label = if show_hidden { " [x] .hidden" } else { " [ ] .hidden" };
                draw_str(buf, w, h, g, toggle_label, px, asc, FG_DIM, px + pw - 1);
                fill(buf, w, h, px, lh - 1, pw - 1, 1, BORDER);

                // Row 1: tree search box
                let ts_empty_str   = String::new();
                let ts_empty_items: Vec<(String, String)> = vec![];
                let (ts_query, ts_focused, ts_fuzzy, ts_items, ts_sel, ts_cursor, ts_sel_anchor) =
                    tree_search_snap.as_ref().map(|s| (&s.0, s.1, s.2, &s.3, s.4, s.5, s.6))
                    .unwrap_or((&ts_empty_str, false, true, &ts_empty_items, 0, 0, None));
                let fuzzy_label = if ts_fuzzy { "[~]" } else { "[=]" };
                let toggle_w = fuzzy_label.chars().count() as i32 * cw + 4;
                draw_str(buf, w, h, g, fuzzy_label, px + 2, lh + asc, FG_DIM, px + toggle_w);
                let sx = px + toggle_w;
                let sw = (pw - toggle_w - 2).max(0);
                if ts_focused { fill(buf, w, h, sx, lh, sw, lh, SEL_BG); }
                let ts_vis = ((sw - 4) / cw).max(0) as usize;
                let ts_hscroll = ts_cursor.saturating_sub(ts_vis.saturating_sub(1));
                let ts_disp: String = ts_query.chars().skip(ts_hscroll).take(ts_vis + 1).collect();
                if ts_focused {
                    if let Some(anc) = ts_sel_anchor {
                        let mn = anc.min(ts_cursor);
                        let mx = anc.max(ts_cursor);
                        if mn < mx {
                            let x0 = sx + 2 + (mn.saturating_sub(ts_hscroll)) as i32 * cw;
                            let x1 = sx + 2 + ((mx - ts_hscroll).min(ts_vis + 1)) as i32 * cw;
                            fill(buf, w, h, x0, lh, (x1 - x0).max(0), lh, SEL_BG);
                        }
                    }
                }
                draw_str(buf, w, h, g, &ts_disp, sx + 2, lh + asc, FG, sx + sw);
                if ts_focused {
                    let cur_x = (sx + 2 + (ts_cursor - ts_hscroll) as i32 * cw).min(sx + sw - 1);
                    fill(buf, w, h, cur_x, lh, 1, lh, ACCENT);
                    fill(buf, w, h, sx, 2 * lh - 1, sw, 1, ACCENT);
                }
                fill(buf, w, h, px, 2 * lh - 1, pw - 1, 1, BORDER);

                // Rows 2+: entries or filtered search results
                let entries_start = 2 * lh;
                if ts_query.is_empty() {
                    for (i, (name, is_dir, expanded, depth, selected)) in entries.iter().enumerate() {
                        let ey = entries_start + (i as i32 - tree_scroll_snap as i32) * lh;
                        if ey + lh <= entries_start { continue; } // scrolled above top
                        if ey + lh > h as i32 - status_h { break; }
                        let baseline = ey + asc;
                        if *selected { fill(buf, w, h, px, ey, pw - 1, lh, SEL_BG); }
                        let prefix = if *is_dir { if *expanded { "▼ " } else { "▶ " } } else { "  " };
                        let indent = px + *depth as i32 * 10 + 4;
                        let label  = format!("{prefix}{name}");
                        draw_str(buf, w, h, g, &label, indent, baseline, FG, px + pw - 1);
                    }
                } else {
                    for (i, (name, dir)) in ts_items.iter().enumerate() {
                        let ey = entries_start + i as i32 * lh;
                        if ey + lh > h as i32 - status_h { break; }
                        let baseline = ey + asc;
                        if i == ts_sel { fill(buf, w, h, px, ey, pw - 1, lh, SEL_BG); }
                        let dir_w = dir.chars().count() as i32 * cw;
                        draw_str(buf, w, h, g, name, px + 4, baseline, FG, px + pw - dir_w - cw - 2);
                        draw_str(buf, w, h, g, dir, px + pw - dir_w - 2, baseline, FG_DIM, px + pw - 1);
                    }
                }
            } else if let Some(ref gf) = gf_snap {
                // Global search panel
                let row_h = lh + 2;
                let field_label_w = 9 * cw;
                let field_x = px + field_label_w;
                let field_w = pw - field_label_w - 4;

                // Helper: draw a labeled input field row
                let mut fy = 4;
                let fields = [
                    ("Query:   ", &gf.query,   GlobalFindFocus::Query,   gf.cursor_query,   gf.sel_q),
                    ("Replace: ", &gf.replace, GlobalFindFocus::Replace, gf.cursor_replace, gf.sel_r),
                    ("Include: ", &gf.include, GlobalFindFocus::Include, gf.cursor_include, gf.sel_inc),
                    ("Exclude: ", &gf.exclude, GlobalFindFocus::Exclude, gf.cursor_exclude, gf.sel_exc),
                ];
                for (label, value, foc, cursor_chars, sel_anc) in &fields {
                    draw_str(buf, w, h, g, label, px + 2, fy + asc, FG_DIM, field_x);
                    let is_active = gf.focus == *foc;
                    let bg = if is_active { SEL_BG } else { BG };
                    fill(buf, w, h, field_x, fy, field_w, lh, bg);
                    if is_active { fill(buf, w, h, field_x, fy + lh - 1, field_w, 1, ACCENT); }
                    let vis = ((field_w - 4) / cw).max(0) as usize;
                    let hscroll = cursor_chars.saturating_sub(vis.saturating_sub(1));
                    if let Some(anc) = sel_anc {
                        let mn = anc.min(cursor_chars);
                        let mx = anc.max(cursor_chars);
                        if mn < mx {
                            let x0 = field_x + 2 + (mn.saturating_sub(hscroll)) as i32 * cw;
                            let x1 = field_x + 2 + ((mx - hscroll).min(vis + 1)) as i32 * cw;
                            fill(buf, w, h, x0, fy, (x1 - x0).max(0), lh, HL_MATCH);
                        }
                    }
                    let disp: String = value.chars().skip(hscroll).take(vis + 1).collect();
                    draw_str(buf, w, h, g, &disp, field_x + 2, fy + asc, FG, field_x + field_w);
                    if is_active {
                        let cx = field_x + 2 + (cursor_chars - hscroll) as i32 * cw;
                        fill(buf, w, h, cx.min(field_x + field_w - 2), fy, 1, lh, ACCENT);
                    }
                    fy += row_h;
                }

                // Buttons row
                fy += 2;
                // Live/Submit toggle (in label area, left of field_x)
                let live_label = if gf.live_search { "[~] live" } else { "[ ] live" };
                let live_col   = if gf.live_search { ACCENT } else { FG_DIM };
                draw_str(buf, w, h, g, live_label, px + 2, fy + asc, live_col, field_x);
                // Search + Replace All buttons
                let search_label = "[Search]";
                let sl = search_label.chars().count() as i32 * cw;
                fill(buf, w, h, field_x, fy, sl, lh, SEL_BG);
                draw_str(buf, w, h, g, search_label, field_x, fy + asc, ACCENT, field_x + sl);
                if !gf.replace.is_empty() && !gf.results.is_empty() {
                    let ra_label = "[Repl All]";
                    let rl = ra_label.chars().count() as i32 * cw;
                    let rx = field_x + sl + cw;
                    fill(buf, w, h, rx, fy, rl, lh, SEL_BG);
                    draw_str(buf, w, h, g, ra_label, rx, fy + asc, ACCENT, rx + rl);
                }
                fy += row_h + 4;
                fill(buf, w, h, px, fy - 1, pw - 1, 1, BORDER);

                // Results
                let res_h = (h as i32 - status_h - fy).max(0);
                let vis_count = (res_h / lh).max(0) as usize;
                let start = gf.scroll.min(gf.results.len());
                let end   = (start + vis_count).min(gf.results.len());
                let count_str = if gf.searching {
                    " Searching...".to_owned()
                } else {
                    format!(" {} match(es)", gf.results.len())
                };
                draw_str(buf, w, h, g, &count_str, px, fy + asc, FG_DIM, px + pw - 1);
                fy += lh;
                for (ri, (name, line_num, line_text, _match_col, _match_len)) in gf.results[start..end].iter().enumerate() {
                    let ry = fy + ri as i32 * lh;
                    if ry + lh > h as i32 - status_h { break; }
                    let is_sel = start + ri == gf.selected;
                    if is_sel {
                        fill(buf, w, h, px, ry, pw - 1, lh, SEL_BG);
                        if sidebar_list_focused { fill(buf, w, h, px, ry, 2, lh, ACCENT); }
                    }
                    let header = format!("{}:{}", name, line_num + 1);
                    draw_str(buf, w, h, g, &header, px + 2, ry + asc, ACCENT, px + pw - 1);
                    let hlen = header.chars().count() as i32 * cw + 4;
                    let preview: String = line_text.trim().chars().take(((pw - hlen) / cw).max(0) as usize).collect();
                    draw_str(buf, w, h, g, &preview, px + 2 + hlen, ry + asc, FG_DIM, px + pw - 1);
                }
            } else if let Some(ref dp) = diag_panel_snap {
                // Diagnostics panel
                let mut ry = 4i32;
                let label = if err_count > 0 {
                    format!(" Diagnostics ({} error{})", err_count, if err_count == 1 { "" } else { "s" })
                } else {
                    " Diagnostics".to_owned()
                };
                draw_str(buf, w, h, g, &label, px, ry + asc, if err_count > 0 { 0xFF5555u32 } else { FG }, px + pw - 1);
                ry += lh + 2;
                fill(buf, w, h, px, ry, pw - 1, 1, BORDER);
                ry += 4;
                for (i, (name, line_num, sev, msg, _path)) in dp.iter().enumerate() {
                    if ry + lh > panel_h { break; }
                    let is_sel = i == diag_panel_sel;
                    if is_sel {
                        fill(buf, w, h, px, ry, pw - 1, lh, SEL_BG);
                        if sidebar_list_focused { fill(buf, w, h, px, ry, 2, lh, ACCENT); }
                    }
                    let (icon, sev_color) = match sev {
                        DiagSeverity::Error   => ("[!]", 0xFF5555u32),
                        DiagSeverity::Warning => ("[~]", 0xE0AF68u32),
                        _                     => ("[-]", FG_DIM),
                    };
                    let header = format!("{} {}:{} ", icon, name, line_num + 1);
                    let hlen   = header.chars().count() as i32 * cw;
                    draw_str(buf, w, h, g, &header, px + 2, ry + asc, sev_color, px + hlen + 2);
                    let avail  = ((pw - hlen - 4) / cw).max(0) as usize;
                    let preview: String = msg.trim().chars().take(avail).collect();
                    draw_str(buf, w, h, g, &preview, px + 2 + hlen, ry + asc, FG_DIM, px + pw - 1);
                    ry += lh;
                }
            } else if let Some(ref gs) = git_snap {
                // Git panel
                let px = act_w;
                let pw = explorer_w;
                if !gs.is_git_repo {
                    if let Some(ref err) = gs.remote_error {
                        draw_str(buf, w, h, g, &format!(" Error: {}", err.lines().next().unwrap_or(err)), px + 2, asc + 4, FG_DIM, px + pw - 1);
                    } else {
                        draw_str(buf, w, h, g, " Not a git repo", px + 2, asc + 4, FG_DIM, px + pw - 1);
                    }
                } else if gs.loading {
                    draw_str(buf, w, h, g, " Loading...", px + 2, asc + 4, FG_DIM, px + pw - 1);
                } else {
                    let scroll_px = gs.scroll as i32 * lh;
                    let mut ry = 4i32 - scroll_px;
                    let commit_area_top = panel_h - lh * 3 - 8;
                    // STAGED header
                    if ry + lh > 0 && ry < commit_area_top {
                        draw_str(buf, w, h, g, " STAGED", px + 2, ry + asc, FG_DIM, px + pw - 1);
                        fill(buf, w, h, px, ry + lh - 1, pw - 1, 1, BORDER);
                    }
                    ry += lh;
                    if gs.staged.is_empty() {
                        if ry + lh > 0 && ry < commit_area_top {
                            draw_str(buf, w, h, g, "  (none)", px + 2, ry + asc, FG_DIM, px + pw - 1);
                        }
                        ry += lh;
                    } else {
                        for (i, entry) in gs.staged.iter().enumerate() {
                            if ry + lh > commit_area_top { break; }
                            if ry + lh > 0 {
                                let is_sel = gs.sel == GitSel::Staged(i);
                                if is_sel { fill(buf, w, h, px, ry, pw - 1, lh, SEL_BG); }
                                let avail = ((pw - 6) / cw).max(0) as usize;
                                let label = format!("  {} {}", entry.xy.0, entry.path);
                                let disp: String = label.chars().take(avail).collect();
                                draw_str(buf, w, h, g, &disp, px + 2, ry + asc, FG, px + pw - 1);
                            }
                            ry += lh;
                        }
                    }

                    // CHANGES header
                    ry += 2;
                    if ry + lh > 0 && ry < commit_area_top {
                        draw_str(buf, w, h, g, " CHANGES", px + 2, ry + asc, FG_DIM, px + pw - 1);
                        fill(buf, w, h, px, ry + lh - 1, pw - 1, 1, BORDER);
                    }
                    ry += lh;
                    if gs.unstaged.is_empty() {
                        if ry + lh > 0 && ry < commit_area_top {
                            draw_str(buf, w, h, g, "  (none)", px + 2, ry + asc, FG_DIM, px + pw - 1);
                        }
                    } else {
                        for (i, entry) in gs.unstaged.iter().enumerate() {
                            if ry + lh > commit_area_top { break; }
                            if ry + lh > 0 {
                                let is_sel = gs.sel == GitSel::Unstaged(i);
                                if is_sel { fill(buf, w, h, px, ry, pw - 1, lh, SEL_BG); }
                                let avail = ((pw - 6) / cw).max(0) as usize;
                                let xy_char = if entry.xy.0 == '?' { '?' } else { entry.xy.1 };
                                let label = format!("  {} {}", xy_char, entry.path);
                                let disp: String = label.chars().take(avail).collect();
                                draw_str(buf, w, h, g, &disp, px + 2, ry + asc, FG, px + pw - 1);
                            }
                            ry += lh;
                        }
                    }

                    // COMMIT area (anchored from bottom)
                    fill(buf, w, h, px, commit_area_top, pw - 1, 1, BORDER);
                    let ca = commit_area_top + 4;
                    draw_str(buf, w, h, g, " COMMIT", px + 2, ca + asc, FG_DIM, px + pw - 1);
                    let ca = ca + lh;
                    // Commit message field
                    let field_bg = if gs.commit_focused { SEL_BG } else { BG };
                    fill(buf, w, h, px + 2, ca, pw - 4, lh, field_bg);
                    let vis = ((pw - 8) / cw).max(0) as usize;
                    let commit_cursor_chars = gs.commit_msg[..gs.commit_cursor].chars().count();
                    let hscroll = commit_cursor_chars.saturating_sub(vis.saturating_sub(1));
                    let disp_msg: String = gs.commit_msg.chars().skip(hscroll).take(vis).collect();
                    if disp_msg.is_empty() && !gs.commit_focused {
                        draw_str(buf, w, h, g, "commit message", px + 4, ca + asc, FG_DIM, px + pw - 3);
                    } else {
                        draw_str(buf, w, h, g, &disp_msg, px + 4, ca + asc, FG, px + pw - 3);
                    }
                    if gs.commit_focused {
                        let cur_col = (commit_cursor_chars - hscroll) as i32;
                        fill(buf, w, h, px + 4 + cur_col * cw, ca, 1, lh, ACCENT);
                        fill(buf, w, h, px + 2, ca + lh - 1, pw - 4, 1, ACCENT);
                    }
                    // Buttons row
                    let btn_y = ca + lh + 2;
                    let can_commit = !gs.staged.is_empty() && !gs.commit_msg.is_empty();
                    let commit_btn_fg = if can_commit { ACCENT } else { FG_DIM };
                    draw_str(buf, w, h, g, "[Commit]", px + 2, btn_y + asc, commit_btn_fg, px + pw / 2);
                    let has_unstaged = !gs.unstaged.is_empty();
                    let stage_all_fg = if has_unstaged { FG } else { FG_DIM };
                    draw_str(buf, w, h, g, "[Stage All]", px + pw / 2, btn_y + asc, stage_all_fg, px + pw - 1);
                }
            }

            // ── Collab panel ──────────────────────────────────────────────
            if left_view == LeftView::Collab {
                let mut cy = 0i32;
                // Header row
                let role_label = if collab_is_host { " 👑 Hosting" } else { " 🔒 Connected" };
                draw_str(buf, w, h, g, role_label, px, cy + asc, ACCENT, px + pw - 1);
                cy += lh;
                fill(buf, w, h, px, cy - 1, pw - 1, 1, BORDER);

                // Invite string (host only)
                if let Some(ref invite) = collab_invite_str {
                    let avail = ((pw - 4) / cw).max(0) as usize;
                    let disp: String = invite.chars().take(avail).collect();
                    draw_str(buf, w, h, g, &disp, px + 2, cy + asc, FG_DIM, px + pw - 1);
                    cy += lh;
                }

                // Peer list header
                fill(buf, w, h, px, cy, pw - 1, lh, BG2);
                draw_str(buf, w, h, g, " Name / File", px, cy + asc, FG_DIM, px + pw * 2 / 3);
                draw_str(buf, w, h, g, " Role", px + pw * 2 / 3, cy + asc, FG_DIM, px + pw - 1);
                cy += lh;
                fill(buf, w, h, px, cy - 1, pw - 1, 1, BORDER);

                if collab_peers_snap.is_empty() {
                    draw_str(buf, w, h, g, " (no peers)", px + 2, cy + asc, FG_DIM, px + pw - 1);
                    cy += lh;
                }
                for (_sid, ref name, color, role, ref cur_file) in &collab_peers_snap {
                    if cy >= panel_h { break; }
                    // Color swatch
                    fill(buf, w, h, px, cy, 3, lh, *color | 0xFF_000000);
                    // Name
                    let name_avail = ((pw * 2 / 3 - 6) / cw).max(0) as usize;
                    let name_disp: String = name.chars().take(name_avail).collect();
                    draw_str(buf, w, h, g, &format!(" {}", name_disp), px + 4, cy + asc, FG, px + pw * 2 / 3);
                    // Role badge
                    let role_lbl = role.label();
                    draw_str(buf, w, h, g, role_lbl, px + pw * 2 / 3, cy + asc, FG_DIM, px + pw - 1);
                    cy += lh;
                    // Current file (dimmed, indented)
                    if !cur_file.is_empty() && cy < panel_h {
                        let fav = ((pw - 8) / cw).max(0) as usize;
                        // Take the basename only to save space
                        let basename = cur_file.rsplit('/').next().unwrap_or(cur_file.as_str());
                        let fdisp: String = basename.chars().take(fav).collect();
                        draw_str(buf, w, h, g, &format!("   {}", fdisp), px, cy + asc, FG_DIM, px + pw - 1);
                        cy += lh;
                    }
                }
            }
            } // end if left_panel_visible
        }

        // ── Per-pane rendering ────────────────────────────────────────────
        for snap in &pane_snaps {
            let r       = snap.rect;
            let scroll  = snap.scroll;
            let hscroll = snap.hscroll;
            let fh      = snap.find_h;
            let gutter_w = snap.gutter_w;
            let ed_x     = r.x + gutter_w;
            let clip_r   = r.x + r.w;

            // Tab bar
            fill(buf, w, h, r.x, r.y, r.w, tab_h, BG2);
            fill(buf, w, h, r.x, r.y + tab_h - 1, r.w, 1, BORDER);
            if snap.is_active {
                fill(buf, w, h, r.x, r.y, 2, tab_h - 1, ACCENT);
            }
            let mut tx = r.x;
            for (i, (name, dirty_tab)) in snap.tab_info.iter().enumerate() {
                let label    = if *dirty_tab { format!(" {}• ", name) } else { format!(" {}  ", name) };
                let tw       = label.chars().count() as i32 * cw;
                let is_act   = i == snap.active_tab;
                let tab_bg   = if is_act { BG } else { BG2 };
                fill(buf, w, h, tx, r.y, tw, tab_h - 1, tab_bg);
                if is_act { fill(buf, w, h, tx, r.y + tab_h - 2, tw, 2, ACCENT); }
                draw_str(buf, w, h, g, &label, tx, r.y + tab_h * 3 / 4, FG, (tx + tw - cw).min(clip_r));
                // × close button (last character cell)
                let x_col = if is_act { FG } else { FG_DIM };
                draw_str(buf, w, h, g, "×", tx + tw - cw, r.y + tab_h * 3 / 4, x_col, (tx + tw).min(clip_r));
                fill(buf, w, h, tx + tw, r.y, 1, tab_h, BORDER);
                tx += tw + 1;
            }

            // Settings tab content
            if snap.is_settings_tab {
                let content_y = r.y + tab_h;
                let scroll_px = snap.scroll as i32 * lh;
                let clip_top  = content_y;
                let clip_bot  = r.y + r.h;
                let btn_x     = r.x + 14 * cw;

                // Clear content area background
                fill(buf, w, h, r.x, clip_top, r.w, clip_bot - clip_top, BG);

                let row_vis = |y: i32| y + lh > clip_top && y < clip_bot;
                // fc = fill clipped to content area
                let fc = |buf: &mut [u32], x: i32, y: i32, rw: i32, rh: i32, color: u32| {
                    fill_clipped(buf, w, h, clip_top, clip_bot, x, y, rw, rh, color);
                };

                // Title separator
                let title_y = content_y - scroll_px;
                if row_vis(title_y) {
                    draw_str(buf, w, h, g, "  Settings", r.x, title_y + asc, FG, r.x + r.w);
                }
                fc(buf, r.x, content_y + lh - scroll_px, r.w, 1, BORDER);

                // Renderer row
                let ry = content_y + lh + 8 - scroll_px;
                if row_vis(ry) {
                    draw_str(buf, w, h, g, "  Renderer", r.x, ry + asc, FG, btn_x - cw);
                    let (cpu_bg, cpu_fg) = if !renderer_is_gpu { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x,          ry, 5 * cw, lh, cpu_bg);
                    draw_str(buf, w, h, g, " CPU ", btn_x,          ry + asc, cpu_fg, btn_x + 5 * cw);
                    let (gpu_bg, gpu_fg) = if  renderer_is_gpu { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x + 6 * cw, ry, 5 * cw, lh, gpu_bg);
                    draw_str(buf, w, h, g, " GPU ", btn_x + 6 * cw, ry + asc, gpu_fg, btn_x + 11 * cw);
                }
                // CPU Double-Buffer row
                let cpu_db_y = ry + lh + 4;
                if row_vis(cpu_db_y) {
                    draw_str(buf, w, h, g, "  CPU Double-Buffer", r.x, cpu_db_y + asc, FG, btn_x - cw);
                    let (db_bg, db_fg) = if cpu_double_buffer { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    let db_label = if cpu_double_buffer { " [x] On " } else { " [ ] Off" };
                    fc(buf, btn_x, cpu_db_y, 8 * cw, lh, db_bg);
                    draw_str(buf, w, h, g, db_label, btn_x, cpu_db_y + asc, db_fg, btn_x + 8 * cw);
                }
                // GPU Drawables row
                let gpu_dc_y = cpu_db_y + lh + 4;
                if row_vis(gpu_dc_y) {
                    draw_str(buf, w, h, g, "  GPU Drawables", r.x, gpu_dc_y + asc, FG, btn_x - cw);
                    for (i, &count) in [2u8, 3u8].iter().enumerate() {
                        let off = i as i32 * 4;
                        let active = gpu_drawable_count == count;
                        let (bg, fg) = if active { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                        let label = if count == 2 { " 2 " } else { " 3 " };
                        fc(buf, btn_x + off * cw, gpu_dc_y, 3 * cw, lh, bg);
                        draw_str(buf, w, h, g, label, btn_x + off * cw, gpu_dc_y + asc, fg, btn_x + (off + 3) * cw);
                    }
                }
                // VSync row
                let vy = gpu_dc_y + lh + 4;
                if row_vis(vy) {
                    draw_str(buf, w, h, g, "  VSync", r.x, vy + asc, FG, btn_x - cw);
                    let (vs_bg, vs_fg) = if vsync_on { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    let vs_label = if vsync_on { " [x] On " } else { " [ ] Off" };
                    fc(buf, btn_x, vy, 8 * cw, lh, vs_bg);
                    draw_str(buf, w, h, g, vs_label, btn_x, vy + asc, vs_fg, btn_x + 8 * cw);
                }
                // UI Scale row
                let sy = vy + lh + 4;
                if row_vis(sy) {
                    draw_str(buf, w, h, g, "  UI Scale", r.x, sy + asc, FG, btn_x - cw);
                    let scale_str = format!("  {:.0}%  ({})", ui_scale * 100.0, key_label("Cmd+= / Cmd+-"));
                    draw_str(buf, w, h, g, &scale_str, btn_x, sy + asc, FG_DIM, r.x + r.w);
                }
                // Rainbow Brackets row
                let rb_y = sy + lh + 4;
                if row_vis(rb_y) {
                    draw_str(buf, w, h, g, "  Rainbow Brackets", r.x, rb_y + asc, FG, btn_x - cw);
                    let (rb_bg, rb_fg) = if rainbow_brackets { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x, rb_y, 8 * cw, lh, rb_bg);
                    draw_str(buf, w, h, g, if rainbow_brackets { " [x] On " } else { " [ ] Off" }, btn_x, rb_y + asc, rb_fg, btn_x + 8 * cw);
                }
                // Glyph Cache row
                let gc_y = rb_y + lh + 4;
                if row_vis(gc_y) {
                    draw_str(buf, w, h, g, "  Glyph Cache", r.x, gc_y + asc, FG, btn_x - cw);
                    // Button offsets (in cw from btn_x), widths, labels — must match GlyphCacheLimit::ALL order
                    let gc_btns: [(i32, i32, &str); 5] = [
                        (0, 11, " Unlimited "),
                        (12, 5, " 512 "),
                        (18, 6, " 1024 "),
                        (25, 6, " 2048 "),
                        (32, 6, " 4096 "),
                    ];
                    for (i, (off, wid, label)) in gc_btns.iter().enumerate() {
                        let active = glyph_cache_limit == settings::GlyphCacheLimit::ALL[i];
                        let (bg, fg) = if active { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                        fc(buf, btn_x + off * cw, gc_y, wid * cw, lh, bg);
                        draw_str(buf, w, h, g, label, btn_x + off * cw, gc_y + asc, fg, btn_x + (off + wid) * cw);
                    }
                }
                // Undo History toggle row
                let ul_y = gc_y + lh + 4;
                if row_vis(ul_y) {
                    draw_str(buf, w, h, g, "  Undo History", r.x, ul_y + asc, FG, btn_x - cw);
                    let unlimited = undo_limit.is_none();
                    let (ul_bg, ul_fg) = if unlimited { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x, ul_y, 12 * cw, lh, ul_bg);
                    draw_str(buf, w, h, g, if unlimited { " [x] Unlimited" } else { " [ ] Unlimited" }, btn_x, ul_y + asc, ul_fg, btn_x + 12 * cw);
                }
                // Undo Limit spinner row (only when limit is Some)
                if let Some(lim) = undo_limit {
                    let ul_num_y = ul_y + lh + 4;
                    if row_vis(ul_num_y) {
                        draw_str(buf, w, h, g, "  Undo Limit", r.x, ul_num_y + asc, FG, btn_x - cw);
                        fc(buf, btn_x, ul_num_y, 3 * cw, lh, SEL_BG);
                        draw_str(buf, w, h, g, " - ", btn_x, ul_num_y + asc, FG, btn_x + 3 * cw);
                        let lim_str = format!("  {}  ", lim);
                        draw_str(buf, w, h, g, &lim_str, btn_x + 3 * cw, ul_num_y + asc, FG_DIM, btn_x + 11 * cw);
                        fc(buf, btn_x + 11 * cw, ul_num_y, 3 * cw, lh, SEL_BG);
                        draw_str(buf, w, h, g, " + ", btn_x + 11 * cw, ul_num_y + asc, FG, btn_x + 14 * cw);
                    }
                }
                // Info row
                let info = if renderer_is_gpu {
                    "  GPU (+~66 MB at 4K) — no tearing at any size"
                } else {
                    "  CPU (no extra RAM) — coalesced + vsync-aligned"
                };
                let info_y = if undo_limit.is_some() { ul_y + lh * 2 + 8 } else { ul_y + lh + 4 };
                if row_vis(info_y) {
                    draw_str(buf, w, h, g, info, r.x, info_y + asc, FG_DIM, r.x + r.w);
                }

                // ── Terminal section ───────────────────────────────────────────
                let term_sec_y = info_y + lh + 8;
                if row_vis(term_sec_y) {
                    draw_str(buf, w, h, g, "  Terminal", r.x, term_sec_y + asc, FG, btn_x - cw);
                }
                fc(buf, r.x + cw, term_sec_y + lh - 1, r.w - 2 * cw, 1, BORDER);

                let tcp_y = term_sec_y + lh + 4;
                if row_vis(tcp_y) {
                    draw_str(buf, w, h, g, &key_label("  Copy/Paste (Cmd+C / Cmd+V)"), r.x, tcp_y + asc, FG, btn_x - cw);
                    let (tcp_bg, tcp_fg) = if term_copy_paste { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x, tcp_y, 8 * cw, lh, tcp_bg);
                    draw_str(buf, w, h, g, if term_copy_paste { " [x] On " } else { " [ ] Off" }, btn_x, tcp_y + asc, tcp_fg, btn_x + 8 * cw);
                }

                let tcb_y = tcp_y + lh + 4;
                if row_vis(tcb_y) {
                    let kill_line = if MACOS { "  Cmd+Backspace \u{2192} \\x15" } else { "  Ctrl+Shift+Backspace \u{2192} \\x15" };
                    draw_str(buf, w, h, g, kill_line, r.x, tcb_y + asc, FG, btn_x - cw);
                    let (tcb_bg, tcb_fg) = if term_cmd_bs { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x, tcb_y, 8 * cw, lh, tcb_bg);
                    draw_str(buf, w, h, g, if term_cmd_bs { " [x] On " } else { " [ ] Off" }, btn_x, tcb_y + asc, tcb_fg, btn_x + 8 * cw);
                }

                let tab_y = tcb_y + lh + 4;
                if row_vis(tab_y) {
                    draw_str(buf, w, h, g, "  Alt+Backspace \u{2192} \\x17", r.x, tab_y + asc, FG, btn_x - cw);
                    let (tab_bg, tab_fg) = if term_alt_bs { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x, tab_y, 8 * cw, lh, tab_bg);
                    draw_str(buf, w, h, g, if term_alt_bs { " [x] On " } else { " [ ] Off" }, btn_x, tab_y + asc, tab_fg, btn_x + 8 * cw);
                }

                let tws_y = tab_y + lh + 4;
                if row_vis(tws_y) {
                    draw_str(buf, w, h, g, "  Double-click", r.x, tws_y + asc, FG, btn_x - cw);
                    let ws_is_active = term_word_select == settings::TermWordSelect::Whitespace;
                    let (ws_bg, ws_fg) = if ws_is_active { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x, tws_y, 11 * cw, lh, ws_bg);
                    draw_str(buf, w, h, g, " Whitespace ", btn_x, tws_y + asc, ws_fg, btn_x + 11 * cw);
                    let (wd_bg, wd_fg) = if !ws_is_active { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x + 12 * cw, tws_y, 6 * cw, lh, wd_bg);
                    draw_str(buf, w, h, g, " Word ", btn_x + 12 * cw, tws_y + asc, wd_fg, btn_x + 18 * cw);
                }

                // ── Language Servers section ───────────────────────────────────
                let lsp_sec_y = tws_y + lh + 8;
                if row_vis(lsp_sec_y) {
                    draw_str(buf, w, h, g, &format!("  Language Servers ({lsp_scope_label})"), r.x, lsp_sec_y + asc, FG, r.x + r.w);
                }
                fc(buf, r.x + cw, lsp_sec_y + lh - 1, r.w - 2 * cw, 1, BORDER);
                let inst_btn_w = 9 * cw;

                for (i, (_, name, status, accent, show_install)) in lsp_rows_snap.iter().enumerate() {
                    let ly = lsp_sec_y + lh + 4 + i as i32 * (lh + 4);
                    if !row_vis(ly) { continue; }
                    draw_str(buf, w, h, g, &format!("  {}", name), r.x, ly + asc, FG, btn_x - cw);
                    if *accent {
                        fc(buf, btn_x, ly, 12 * cw, lh, ACCENT);
                        draw_str(buf, w, h, g, &format!(" {:<10}", status), btn_x, ly + asc, BG, btn_x + 12 * cw);
                    } else {
                        draw_str(buf, w, h, g, &format!(" {:<11}", status), btn_x, ly + asc, FG_DIM, btn_x + 13 * cw);
                    }
                    if *show_install {
                        fc(buf, btn_x + 14 * cw, ly, inst_btn_w, lh, SEL_BG);
                        draw_str(buf, w, h, g, "[Install]", btn_x + 14 * cw, ly + asc, FG, btn_x + 14 * cw + inst_btn_w);
                    }
                }

                // ── Save Actions section ───────────────────────────────────────
                let save_sec_y = lsp_sec_y + lh + 4 + 3 * (lh + 4) + 4;
                if row_vis(save_sec_y) {
                    draw_str(buf, w, h, g, "  Save Actions", r.x, save_sec_y + asc, FG, btn_x - cw);
                }
                fc(buf, r.x + cw, save_sec_y + lh - 1, r.w - 2 * cw, 1, BORDER);

                let field_w = (r.w - 16 * cw - 4).max(cw);
                let save_btn_x = r.x + 16 * cw;

                let sa_fields: [(&str, SettingsFieldId, &str); 3] = [
                    ("  Format on save",   SettingsFieldId::FormatOnSave,          &format_on_save),
                    ("  Organize imports", SettingsFieldId::OrganizeImportsOnSave, &organize_imports_on_save),
                    ("  Custom formatter", SettingsFieldId::FormatCommand,         &format_command),
                ];
                for (i, (label, fid, value)) in sa_fields.iter().enumerate() {
                    let fy = save_sec_y + lh + 4 + i as i32 * (lh + 4);
                    if !row_vis(fy) { continue; }
                    draw_str(buf, w, h, g, label, r.x, fy + asc, FG, save_btn_x - cw);
                    let is_focused = settings_edit_field == Some(*fid);
                    let display = if is_focused { settings_edit_text.as_str() } else { *value };
                    let field_bg = if is_focused { SEL_BG } else { BG2 };
                    fc(buf, save_btn_x, fy, field_w, lh, field_bg);
                    draw_str(buf, w, h, g, display, save_btn_x + cw, fy + asc, FG, save_btn_x + field_w - cw);
                    if is_focused {
                        let cur_x = save_btn_x + cw + settings_edit_cursor as i32 * cw;
                        fc(buf, cur_x, fy + 1, 1, lh - 2, FG);
                    }
                }

                // ── Collaboration section ─────────────────────────────────────
                let collab_sec_y = save_sec_y + lh + 4 + 3 * (lh + 4) + 8;
                if row_vis(collab_sec_y) {
                    draw_str(buf, w, h, g, "  Collaboration", r.x, collab_sec_y + asc, FG, r.x + r.w);
                }
                fc(buf, r.x + cw, collab_sec_y + lh - 1, r.w - 2 * cw, 1, BORDER);

                // Port
                let cp_y = collab_sec_y + lh + 4;
                if row_vis(cp_y) {
                    draw_str(buf, w, h, g, "  Collab port", r.x, cp_y + asc, FG, save_btn_x - cw);
                    let is_focused = settings_edit_field == Some(SettingsFieldId::CollabPort);
                    let port_str_owned = collab_port.to_string();
                    let display = if is_focused { settings_edit_text.as_str() } else { &port_str_owned };
                    let port_w = 6 * cw;
                    let field_bg = if is_focused { SEL_BG } else { BG2 };
                    fc(buf, save_btn_x, cp_y, port_w, lh, field_bg);
                    draw_str(buf, w, h, g, display, save_btn_x + cw, cp_y + asc, FG, save_btn_x + port_w - cw);
                    if is_focused {
                        let cur_x = save_btn_x + cw + settings_edit_cursor as i32 * cw;
                        fc(buf, cur_x, cp_y + 1, 1, lh - 2, FG);
                    }
                }

                // Default role
                let dr_y = cp_y + lh + 4;
                if row_vis(dr_y) {
                    draw_str(buf, w, h, g, "  Default role", r.x, dr_y + asc, FG, save_btn_x - cw);
                    let role_labels = [
                        (collab::PeerRole::Viewer,       " Viewer "),
                        (collab::PeerRole::Contributor,  " Contributor "),
                        (collab::PeerRole::SystemAccess, " Sys.Access "),
                        (collab::PeerRole::Moderator,    " Moderator "),
                    ];
                    let mut rx2 = save_btn_x;
                    for (role, label) in role_labels {
                        let rw2 = label.chars().count() as i32 * cw;
                        let active = collab_role_perms.default_role == role;
                        let (rbg, rfg) = if active { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                        fc(buf, rx2, dr_y, rw2, lh, rbg);
                        draw_str(buf, w, h, g, label, rx2, dr_y + asc, rfg, rx2 + rw2);
                        rx2 += rw2 + cw;
                    }
                }

                // Include globs
                let cig_y = dr_y + lh + 4;
                if row_vis(cig_y) {
                    draw_str(buf, w, h, g, "  Include globs", r.x, cig_y + asc, FG, save_btn_x - cw);
                    let is_focused = settings_edit_field == Some(SettingsFieldId::CollabIncludeGlobs);
                    let display = if is_focused { settings_edit_text.as_str() } else { &collab_include_globs };
                    let field_bg = if is_focused { SEL_BG } else { BG2 };
                    fc(buf, save_btn_x, cig_y, field_w, lh, field_bg);
                    draw_str(buf, w, h, g, display, save_btn_x + cw, cig_y + asc, FG, save_btn_x + field_w - cw);
                    if is_focused {
                        let cur_x = save_btn_x + cw + settings_edit_cursor as i32 * cw;
                        fc(buf, cur_x, cig_y + 1, 1, lh - 2, FG);
                    }
                }

                // Exclude globs
                let ceg_y = cig_y + lh + 4;
                if row_vis(ceg_y) {
                    draw_str(buf, w, h, g, "  Exclude globs", r.x, ceg_y + asc, FG, save_btn_x - cw);
                    let is_focused = settings_edit_field == Some(SettingsFieldId::CollabExcludeGlobs);
                    let display = if is_focused { settings_edit_text.as_str() } else { &collab_exclude_globs };
                    let field_bg = if is_focused { SEL_BG } else { BG2 };
                    fc(buf, save_btn_x, ceg_y, field_w, lh, field_bg);
                    draw_str(buf, w, h, g, display, save_btn_x + cw, ceg_y + asc, FG, save_btn_x + field_w - cw);
                    if is_focused {
                        let cur_x = save_btn_x + cw + settings_edit_cursor as i32 * cw;
                        fc(buf, cur_x, ceg_y + 1, 1, lh - 2, FG);
                    }
                }

                // Permission matrix header
                let perm_hdr_y = ceg_y + lh + 8;
                let perm_col_labels = ["vw", "wt", "hv", "hw", "tv", "to", "ro"];
                let perm_col_w = 4 * cw;
                if row_vis(perm_hdr_y) {
                    draw_str(buf, w, h, g, "  Permissions", r.x, perm_hdr_y + asc, FG_DIM, save_btn_x - cw);
                    for (i, label) in perm_col_labels.iter().enumerate() {
                        let px2 = save_btn_x + i as i32 * perm_col_w;
                        draw_str(buf, w, h, g, label, px2, perm_hdr_y + asc, FG_DIM, px2 + perm_col_w);
                    }
                }
                let perm_bits = [
                    collab::perms::VIEW_FILES, collab::perms::WRITE_FILES,
                    collab::perms::VIEW_HIDDEN, collab::perms::WRITE_HIDDEN,
                    collab::perms::VIEW_TERMINALS, collab::perms::OPEN_TERMINALS,
                    collab::perms::MANAGE_ROLES,
                ];
                let perm_rows = [
                    (collab::PeerRole::Viewer,       "  Viewer",
                     collab_role_perms.viewer),
                    (collab::PeerRole::Contributor,  "  Contributor",
                     collab_role_perms.contributor),
                    (collab::PeerRole::SystemAccess, "  Sys.Access",
                     collab_role_perms.system_access),
                    (collab::PeerRole::Moderator,    "  Moderator",
                     collab_role_perms.moderator),
                ];
                for (ri, (_role, label, bits)) in perm_rows.iter().enumerate() {
                    let pm_y = perm_hdr_y + lh + 4 + ri as i32 * (lh + 2);
                    if !row_vis(pm_y) { continue; }
                    draw_str(buf, w, h, g, label, r.x, pm_y + asc, FG, save_btn_x - cw);
                    for (ci, &bit) in perm_bits.iter().enumerate() {
                        let px2 = save_btn_x + ci as i32 * perm_col_w;
                        let has = bits & bit != 0;
                        let (pbg, pfg) = if has { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                        fc(buf, px2, pm_y, 3 * cw, lh, pbg);
                        draw_str(buf, w, h, g, if has { "[x]" } else { "[ ]" }, px2, pm_y + asc, pfg, px2 + 3 * cw);
                    }
                }

                // Re-paint tab bar on top to overwrite any content that bled upward when scrolled
                fill(buf, w, h, r.x, r.y, r.w, tab_h, BG2);
                fill(buf, w, h, r.x, r.y + tab_h - 1, r.w, 1, BORDER);
                if snap.is_active { fill(buf, w, h, r.x, r.y, 2, tab_h - 1, ACCENT); }
                let mut tx2 = r.x;
                for (i, (name, dirty_tab)) in snap.tab_info.iter().enumerate() {
                    let label  = if *dirty_tab { format!(" {}• ", name) } else { format!(" {}  ", name) };
                    let tw     = label.chars().count() as i32 * cw;
                    let is_act = i == snap.active_tab;
                    fill(buf, w, h, tx2, r.y, tw, tab_h - 1, if is_act { BG } else { BG2 });
                    if is_act { fill(buf, w, h, tx2, r.y + tab_h - 2, tw, 2, ACCENT); }
                    draw_str(buf, w, h, g, &label, tx2, r.y + tab_h * 3 / 4, FG, (tx2 + tw - cw).min(clip_r));
                    draw_str(buf, w, h, g, "×", tx2 + tw - cw, r.y + tab_h * 3 / 4, if is_act { FG } else { FG_DIM }, (tx2 + tw).min(clip_r));
                    fill(buf, w, h, tx2 + tw, r.y, 1, tab_h, BORDER);
                    tx2 += tw + 1;
                }

                continue;
            }

            // GitDiff tab content
            if snap.is_git_diff_tab {
                let content_y = r.y + tab_h;
                let content_h = r.y + r.h - content_y;
                fill(buf, w, h, r.x, content_y, r.w, content_h, BG);

                if let Some((ref lines, gd_scroll, gd_loading)) = snap.git_diff_snap {
                    const DIGITS: i32 = 4;
                    let gutter_w = (DIGITS * 2 + 3) * cw;
                    let ed_gx = r.x + gutter_w;
                    let clip_r_gd = r.x + r.w;

                    // Vertical separator lines in gutter
                    let sep1_x = r.x + (DIGITS + 1) * cw;
                    let sep2_x = r.x + (DIGITS * 2 + 2) * cw;
                    fill(buf, w, h, sep1_x, content_y, 1, content_h, BORDER);
                    fill(buf, w, h, sep2_x, content_y, 1, content_h, BORDER);

                    if gd_loading {
                        draw_str(buf, w, h, g, "  Loading...", ed_gx, content_y + asc, FG_DIM, clip_r_gd);
                    } else if lines.is_empty() {
                        draw_str(buf, w, h, g, "  (no changes)", ed_gx, content_y + asc, FG_DIM, clip_r_gd);
                    } else {
                        let (mut old_line, mut new_line) = compute_line_nums_at(lines, gd_scroll);
                        let mut ry = content_y;
                        for line in lines.iter().skip(gd_scroll) {
                            if ry + lh > r.y + r.h { break; }
                            match line {
                                DiffLine::Hunk(s) => {
                                    fill(buf, w, h, r.x, ry, r.w, lh, BG);
                                    draw_str(buf, w, h, g, s, ed_gx, ry + asc, DIFF_HUNK, clip_r_gd);
                                    if let Some((o, n)) = parse_hunk_header(s) { old_line = o; new_line = n; }
                                }
                                DiffLine::Added(s) => {
                                    fill(buf, w, h, r.x, ry, r.w, lh, DIFF_ADD_BG);
                                    let ln_str = format!("{:>w$}", new_line, w = DIGITS as usize);
                                    draw_str(buf, w, h, g, &ln_str, r.x + (DIGITS + 2) * cw, ry + asc, DIFF_ADD_FG, sep2_x);
                                    draw_str(buf, w, h, g, s, ed_gx, ry + asc, DIFF_ADD_FG, clip_r_gd);
                                    new_line += 1;
                                }
                                DiffLine::Removed(s) => {
                                    fill(buf, w, h, r.x, ry, r.w, lh, DIFF_DEL_BG);
                                    let ln_str = format!("{:>w$}", old_line, w = DIGITS as usize);
                                    draw_str(buf, w, h, g, &ln_str, r.x + cw, ry + asc, DIFF_DEL_FG, sep1_x);
                                    draw_str(buf, w, h, g, s, ed_gx, ry + asc, DIFF_DEL_FG, clip_r_gd);
                                    old_line += 1;
                                }
                                DiffLine::Context(s) => {
                                    let old_str = format!("{:>w$}", old_line, w = DIGITS as usize);
                                    let new_str = format!("{:>w$}", new_line, w = DIGITS as usize);
                                    draw_str(buf, w, h, g, &old_str, r.x + cw, ry + asc, FG_DIM, sep1_x);
                                    draw_str(buf, w, h, g, &new_str, r.x + (DIGITS + 2) * cw, ry + asc, FG_DIM, sep2_x);
                                    draw_str(buf, w, h, g, s, ed_gx, ry + asc, FG_DIM, clip_r_gd);
                                    old_line += 1;
                                    new_line += 1;
                                }
                            }
                            ry += lh;
                        }
                    }
                }
                continue;
            }

            // Editor lines
            for (vi, (text, line_start, colors)) in snap.lines.iter().enumerate() {
                let li       = scroll + vi;
                let py       = r.y + tab_h + vi as i32 * lh;
                let baseline = py + asc;

                // Line number (right-aligned in gutter, colored by diagnostic severity)
                let ln_str  = (li + 1).to_string();
                let ln_x    = r.x + (snap.ln_digits as i32 - ln_str.len() as i32) * cw;
                let is_cur  = snap.cursors_snap.iter().any(|&(l, _, _)| l == li);
                let ln_color = if is_cur {
                    FG
                } else {
                    // Check if any diagnostic is on this line; use worst severity color
                    let has_error = snap.diagnostics.iter().any(|&(dl, _, _, ref s, _)| dl == li && *s == DiagSeverity::Error);
                    let has_warn  = snap.diagnostics.iter().any(|&(dl, _, _, ref s, _)| dl == li && *s == DiagSeverity::Warning);
                    if has_error { 0xFF5555u32 } else if has_warn { 0xE0AF68 } else { FG_DIM }
                };
                draw_str(buf, w, h, g, &ln_str, ln_x, baseline, ln_color, ed_x);

                // Find match highlights
                for &(mlo, mhi) in &snap.match_ranges {
                    let lcc      = text.chars().count();
                    let line_end = line_start + lcc;
                    if mlo < line_end + 1 && mhi > *line_start {
                        let is_act = snap.cursors_snap.iter().any(|(_, _, sel)| {
                            sel.map_or(false, |(lo, hi)| lo == mlo && hi == mhi)
                        });
                        let color  = if is_act { HL_MATCH_ACTIVE } else { HL_MATCH };
                        let col_lo = mlo.saturating_sub(*line_start);
                        let col_hi = mhi.saturating_sub(*line_start).min(lcc);
                        let sx     = (ed_x + (col_lo as i32 - hscroll as i32) * cw).max(ed_x);
                        let ex     = (ed_x + (col_hi as i32 - hscroll as i32) * cw).min(clip_r);
                        let sw     = (ex - sx).max(0);
                        if sw > 0 { fill(buf, w, h, sx, py, sw, lh, color); }
                    }
                }

                // Selection highlights
                for &(_, _, sel) in &snap.cursors_snap {
                    if let Some((sel_lo, sel_hi)) = sel {
                        let lcc      = text.chars().count();
                        let line_end = line_start + lcc;
                        if sel_lo < line_end + 1 && sel_hi > *line_start {
                            let col_lo = sel_lo.saturating_sub(*line_start);
                            let col_hi = if sel_hi > line_end { lcc + 1 } else { sel_hi - line_start };
                            let sx_raw = ed_x + (col_lo as i32 - hscroll as i32) * cw;
                            let sx_end = (ed_x + (col_hi as i32 - hscroll as i32) * cw).min(clip_r);
                            let sx     = sx_raw.max(ed_x);
                            let sw     = (sx_end - sx).max(0);
                            if sw > 0 { fill(buf, w, h, sx, py, sw, lh, SEL_BG); }
                        }
                    }
                }

                // Text
                let mut x = ed_x - hscroll as i32 * cw;
                for (ci, ch) in text.chars().enumerate() {
                    if x + cw > ed_x && x < clip_r {
                        let color = colors.get(ci).copied().unwrap_or(FG);
                        if let Some((m, bmap)) = g.get(ch) {
                            blit(buf, w, h, bmap, m, x, baseline, color);
                        }
                    }
                    x += cw;
                    if x >= clip_r { break; }
                }

                // Cursors
                for &(c_line, c_col, _) in &snap.cursors_snap {
                    if c_line == li && cursor_visible {
                        let cx = ed_x + (c_col as i32 - hscroll as i32) * cw;
                        if cx >= ed_x && cx < clip_r { fill(buf, w, h, cx, py, 2, lh, ACCENT); }
                    }
                }

                // Remote collab cursors (3-pixel bar in peer color)
                for &(rc_line, rc_col, rc_color) in &snap.remote_cursors {
                    if rc_line == li {
                        let cx = ed_x + (rc_col as i32 - hscroll as i32) * cw;
                        if cx >= ed_x && cx < clip_r { fill(buf, w, h, cx, py, 3, lh, rc_color); }
                    }
                }

                // Diagnostic squiggles
                for &(dline, cs, ce, ref sev, _) in &snap.diagnostics {
                    if dline != li { continue; }
                    let color = match sev {
                        DiagSeverity::Error   => 0xFF5555u32,
                        DiagSeverity::Warning => 0xE0AF68,
                        _                     => FG_DIM,
                    };
                    let x1 = (ed_x + (cs as i32 - hscroll as i32) * cw).max(ed_x);
                    let x2 = ed_x + (ce as i32 - hscroll as i32) * cw;
                    let sq_w = (x2 - x1).max(cw).min(clip_r - x1);
                    if sq_w > 0 { draw_squiggle(buf, w, h, x1, baseline + 2, sq_w, color); }
                }
            }

            // Scrollbars
            let total  = snap.total;
            let vis    = (snap.editor_h / lh).max(1) as usize;
            let editor_w = r.w;

            if total > vis {
                let track_h = snap.editor_h;
                let thumb_h = ((track_h * vis as i32) / total as i32).max(SB_W);
                let thumb_y = r.y + tab_h + ((scroll as i32 * (track_h - thumb_h)) / (total - vis) as i32);
                fill(buf, w, h, r.x + r.w - SB_W, r.y + tab_h, SB_W, track_h, BG2);
                fill(buf, w, h, r.x + r.w - SB_W, thumb_y, SB_W, thumb_h, SB_THUMB);
            }

            let text_area_w = editor_w - gutter_w;
            let vis_cols = (text_area_w / cw).max(1) as usize;
            if snap.max_line_len > vis_cols {
                let track_w = text_area_w - if total > vis { SB_W } else { 0 };
                let thumb_w = ((track_w * vis_cols as i32) / snap.max_line_len as i32).max(SB_W);
                let thumb_x = ed_x + ((hscroll as i32 * (track_w - thumb_w)) / (snap.max_line_len - vis_cols) as i32);
                let sb_y    = r.y + r.h - fh - SB_W;
                fill(buf, w, h, r.x, sb_y, track_w, SB_W, BG2);
                fill(buf, w, h, thumb_x, sb_y, thumb_w, SB_W, SB_THUMB);
            }

            // Find bar
            if snap.find_open {
                let row_h  = lh + 4;
                let find_y = r.y + r.h - fh;
                fill(buf, w, h, r.x, find_y, r.w, fh, BG2);
                fill(buf, w, h, r.x, find_y, r.w, 1, BORDER);

                let row1_base = find_y + row_h * 3 / 4;
                let label     = "Find: ";
                let lw        = label.len() as i32 * cw;
                draw_str(buf, w, h, g, label, r.x + 4, row1_base, FG_DIM, clip_r);

                let aa_str   = if snap.case_sensitive { "[Aa]" } else { "[aa]" };
                let ww_str   = if snap.whole_word     { "[W]"  } else { "[w]"  };
                let toggle_w = (aa_str.len() + 1 + ww_str.len()) as i32 * cw + 8;
                let aa_x     = clip_r - toggle_w;
                let ww_x     = aa_x + (aa_str.len() + 1) as i32 * cw;
                draw_str(buf, w, h, g, aa_str, aa_x, row1_base,
                         if snap.case_sensitive { ACCENT } else { FG_DIM }, clip_r);
                draw_str(buf, w, h, g, ww_str, ww_x, row1_base,
                         if snap.whole_word     { ACCENT } else { FG_DIM }, clip_r);

                let mc_str = format!("{} matches", snap.match_ranges.len());
                let mc_w   = mc_str.len() as i32 * cw;
                let mc_x   = aa_x - mc_w - cw;
                draw_str(buf, w, h, g, &mc_str, mc_x, row1_base, FG_DIM, mc_x + mc_w + cw);

                let qx    = r.x + 4 + lw;
                let qclip = mc_x - cw;
                let q_vis = ((qclip - qx) / cw).max(0) as usize;
                let q_hscroll = snap.find_cursor_q.saturating_sub(q_vis.saturating_sub(1));
                if let Some(anc) = snap.find_sel_q {
                    let mn = anc.min(snap.find_cursor_q);
                    let mx = anc.max(snap.find_cursor_q);
                    if mn < mx {
                        let x0 = (qx + (mn.saturating_sub(q_hscroll)) as i32 * cw).min(qclip);
                        let x1 = (qx + ((mx - q_hscroll).min(q_vis + 1)) as i32 * cw).min(qclip);
                        fill(buf, w, h, x0, find_y + 2, (x1 - x0).max(0), lh, HL_MATCH);
                    }
                }
                let q_disp: String = snap.find_query.chars().skip(q_hscroll).take(q_vis + 1).collect();
                draw_str(buf, w, h, g, &q_disp, qx, row1_base, FG, qclip);
                if snap.find_input_focused && snap.find_focus == FindFocus::Query {
                    let cx = qx + (snap.find_cursor_q - q_hscroll) as i32 * cw;
                    if cx < qclip { fill(buf, w, h, cx, find_y + 2, 2, lh, ACCENT); }
                }

                if snap.find_repl_open {
                    let row2_y    = find_y + row_h;
                    let row2_base = row2_y + row_h * 3 / 4;
                    fill(buf, w, h, r.x, row2_y, r.w, 1, BORDER);

                    let rlabel = "Replace: ";
                    let rlw    = rlabel.len() as i32 * cw;
                    draw_str(buf, w, h, g, rlabel, r.x + 4, row2_base, FG_DIM, clip_r);

                    let repl_str = "[Repl]";
                    let all_str  = "[All]";
                    let btn_w    = (repl_str.len() + 1 + all_str.len()) as i32 * cw + 8;
                    let btn_x    = clip_r - btn_w;
                    let all_x    = btn_x + (repl_str.len() + 1) as i32 * cw;
                    draw_str(buf, w, h, g, repl_str, btn_x, row2_base, FG_DIM, clip_r);
                    draw_str(buf, w, h, g, all_str,  all_x, row2_base, FG_DIM, clip_r);

                    let rx    = r.x + 4 + rlw;
                    let rclip = btn_x - cw;
                    let r_vis = ((rclip - rx) / cw).max(0) as usize;
                    let r_hscroll = snap.find_cursor_r.saturating_sub(r_vis.saturating_sub(1));
                    if let Some(anc) = snap.find_sel_r {
                        let mn = anc.min(snap.find_cursor_r);
                        let mx = anc.max(snap.find_cursor_r);
                        if mn < mx {
                            let x0 = (rx + (mn.saturating_sub(r_hscroll)) as i32 * cw).min(rclip);
                            let x1 = (rx + ((mx - r_hscroll).min(r_vis + 1)) as i32 * cw).min(rclip);
                            fill(buf, w, h, x0, row2_y + 2, (x1 - x0).max(0), lh, HL_MATCH);
                        }
                    }
                    let r_disp: String = snap.find_repl.chars().skip(r_hscroll).take(r_vis + 1).collect();
                    draw_str(buf, w, h, g, &r_disp, rx, row2_base, FG, rclip);
                    if snap.find_input_focused && snap.find_focus == FindFocus::Replace {
                        let cx = rx + (snap.find_cursor_r - r_hscroll) as i32 * cw;
                        if cx < rclip { fill(buf, w, h, cx, row2_y + 2, 2, lh, ACCENT); }
                    }
                }
            }
        }

        // ── Terminal panes ────────────────────────────────────────────────
        for snap in &term_snaps {
            let r = snap.rect;
            // Tab bar
            fill(buf, w, h, r.x, r.y, r.w, tab_h, BG2);
            fill(buf, w, h, r.x, r.y + tab_h - 1, r.w, 1, BORDER);
            if snap.is_active { fill(buf, w, h, r.x, r.y, 2, tab_h - 1, ACCENT); }
            let mut tx = r.x;
            for (i, title) in snap.tabs.iter().enumerate() {
                let label  = format!(" {}  ", title);
                let tw     = label.chars().count() as i32 * cw;
                let is_act = i == snap.active_tab;
                fill(buf, w, h, tx, r.y, tw, tab_h - 1, if is_act { BG } else { BG2 });
                if is_act { fill(buf, w, h, tx, r.y + tab_h - 2, tw, 2, ACCENT); }
                draw_str(buf, w, h, g, &label, tx, r.y + tab_h * 3 / 4, FG,
                    (tx + tw - cw).min(r.x + r.w));
                draw_str(buf, w, h, g, "×", tx + tw - cw, r.y + tab_h * 3 / 4,
                    if is_act { FG } else { FG_DIM }, (tx + tw).min(r.x + r.w));
                fill(buf, w, h, tx + tw, r.y, 1, tab_h, BORDER);
                tx += tw + 1;
            }
            // Grid rows
            for (vi, row) in snap.visible_rows.iter().enumerate() {
                let py       = r.y + tab_h + vi as i32 * lh;
                if py + lh > r.y + r.h { break; }  // clip to pane bottom
                let baseline = py + asc;
                for (ci, cell) in row.iter().enumerate() {
                    let cx = r.x + ci as i32 * cw;
                    if cx >= r.x + r.w { break; }
                    let in_sel = snap.sel.as_ref().map_or(false, |s| s.contains(vi, ci));
                    let bg = if in_sel { SEL_BG } else { cell.bg };
                    if bg != BG { fill(buf, w, h, cx, py, cw, lh, bg); }
                    if cell.ch != ' ' {
                        if let Some((m, bmap)) = g.get(cell.ch) {
                            blit(buf, w, h, bmap, m, cx, baseline, cell.fg);
                        }
                    }
                }
            }
            // Cursor (block, 2px wide)
            if snap.is_active && snap.cursor_visible {
                let cx = r.x + snap.cursor_col as i32 * cw;
                let cy = r.y + tab_h + snap.cursor_row as i32 * lh;
                if cy + lh <= r.y + r.h {  // clip cursor to pane bottom
                    fill(buf, w, h, cx, cy, 2, lh, ACCENT);
                }
            }
        }

        // ── Markdown preview panes ────────────────────────────────────────
        for snap in &md_snaps {
            let r = snap.rect;
            fill(buf, w, h, r.x, r.y, r.w, tab_h, BG2);
            fill(buf, w, h, r.x, r.y + tab_h - 1, r.w, 1, BORDER);
            if snap.is_active { fill(buf, w, h, r.x, r.y, 2, tab_h - 1, ACCENT); }
            draw_str(buf, w, h, g, &format!(" {}", snap.title), r.x + 4, r.y + tab_h * 3 / 4, FG_DIM, r.x + r.w);
            for (vi, (text, colors)) in snap.lines.iter().enumerate() {
                let baseline = r.y + tab_h + vi as i32 * lh + asc;
                let mut x = r.x + 4;
                for (ci, ch) in text.chars().enumerate() {
                    if x >= r.x + r.w { break; }
                    let color = colors.get(ci).copied().unwrap_or(FG);
                    if let Some((m, bmap)) = g.get(ch) { blit(buf, w, h, bmap, m, x, baseline, color); }
                    x += cw;
                }
            }
            let content_h = (r.h - tab_h).max(0);
            let vis = (content_h / lh).max(1) as usize;
            if snap.total_lines > vis {
                let thumb_h = ((content_h * vis as i32) / snap.total_lines as i32).max(SB_W);
                let scroll_max = snap.total_lines - vis;
                let thumb_y = r.y + tab_h + if scroll_max > 0 {
                    (snap.scroll as i32 * (content_h - thumb_h)) / scroll_max as i32
                } else { 0 };
                fill(buf, w, h, r.x + r.w - SB_W, r.y + tab_h, SB_W, content_h, BG2);
                fill(buf, w, h, r.x + r.w - SB_W, thumb_y, SB_W, thumb_h, SB_THUMB);
            }
        }

        // ── Pane border dividers ──────────────────────────────────────────
        for snap in &pane_snaps {
            // 1px right border (between H-split panes)
            fill(buf, w, h, snap.rect.x + snap.rect.w, snap.rect.y, 1, snap.rect.h, BORDER);
            // 1px bottom border (between V-split panes)
            fill(buf, w, h, snap.rect.x, snap.rect.y + snap.rect.h, snap.rect.w, 1, BORDER);
        }
        // Also draw borders for terminal and output panes
        for snap in &term_snaps {
            fill(buf, w, h, snap.rect.x + snap.rect.w, snap.rect.y, 1, snap.rect.h, BORDER);
            fill(buf, w, h, snap.rect.x, snap.rect.y + snap.rect.h, snap.rect.w, 1, BORDER);
        }
        for snap in &md_snaps {
            fill(buf, w, h, snap.rect.x + snap.rect.w, snap.rect.y, 1, snap.rect.h, BORDER);
            fill(buf, w, h, snap.rect.x, snap.rect.y + snap.rect.h, snap.rect.w, 1, BORDER);
        }

        // ── Drag drop zone overlay ────────────────────────────────────────
        if let Some((_, Some(over_id), Some(zone))) = drag_snap {
            let target_rect = if drag_src_is_terminal {
                term_snaps.iter().find(|t| t.id == over_id).map(|t| t.rect)
                    .or_else(|| pane_snaps.iter().find(|p| p.id == over_id).map(|p| p.rect))
            } else {
                pane_snaps.iter().find(|p| p.id == over_id).map(|p| p.rect)
                    .or_else(|| term_snaps.iter().find(|t| t.id == over_id).map(|t| t.rect))
            };
            if let Some(r) = target_rect {
                let th = tab_h;
                let zone_rect = match zone {
                    DropZone::Center => Rect { x: r.x + r.w/4,       y: r.y + th + (r.h-th)/4,       w: r.w/2,     h: (r.h-th)/2 },
                    DropZone::Left   => Rect { x: r.x,               y: r.y + th,                     w: r.w/4,     h: r.h - th },
                    DropZone::Right  => Rect { x: r.x + r.w*3/4,     y: r.y + th,                     w: r.w/4,     h: r.h - th },
                    DropZone::Top    => Rect { x: r.x,               y: r.y + th,                     w: r.w,       h: (r.h-th)/4 },
                    DropZone::Bottom => Rect { x: r.x,               y: r.y + th + (r.h-th)*3/4,      w: r.w,       h: (r.h-th)/4 },
                };
                fill(buf, w, h, zone_rect.x, zone_rect.y, zone_rect.w, zone_rect.h, DRAG_ZONE);
                fill(buf, w, h, zone_rect.x,               zone_rect.y,                zone_rect.w, 1,            ACCENT);
                fill(buf, w, h, zone_rect.x,               zone_rect.y + zone_rect.h-1, zone_rect.w, 1,           ACCENT);
                fill(buf, w, h, zone_rect.x,               zone_rect.y,                1, zone_rect.h,            ACCENT);
                fill(buf, w, h, zone_rect.x + zone_rect.w-1, zone_rect.y,              1, zone_rect.h,            ACCENT);
            }
        }

        // ── Status bar ────────────────────────────────────────────────────
        let sy = h as i32 - status_h;
        fill(buf, w, h, 0, sy, w as i32, status_h, BG2);
        fill(buf, w, h, 0, sy, w as i32, 1, BORDER);

        let sbase = sy + status_h * 3 / 4;
        // Show collab indicator on the right of the status bar (if active)
        if let Some(ref cs) = collab_status {
            let cs_w = cs.chars().count() as i32 * cw;
            draw_str(buf, w, h, g, cs, w as i32 - cs_w, sbase, 0x6BCB77, w as i32);
        }
        let right_margin = collab_status.as_ref().map_or(0, |cs| cs.chars().count() as i32 * cw);
        if let Some(msg) = &status_msg {
            draw_str(buf, w, h, g, &format!("  {msg}"), 0, sbase, 0xFFB86C, w as i32 - right_margin);
        } else if let Some(snap) = pane_snaps.iter().find(|p| p.is_active) {
            let dirty_mark = if snap.dirty { " *" } else { "" };
            draw_str(buf, w, h, g, &format!("  {}{dirty_mark}", snap.path_name), 0, sbase, FG, w as i32 - right_margin);
            if collab_status.is_none() {
                let lc_str = format!("Ln {}, Col {}  ", snap.cur_line + 1, snap.cur_col + 1);
                let lc_w   = lc_str.chars().count() as i32 * cw;
                draw_str(buf, w, h, g, &lc_str, w as i32 - lc_w, sbase, FG_DIM, w as i32);
            }
        } else if let Some(snap) = md_snaps.iter().find(|p| p.is_active) {
            draw_str(buf, w, h, g, &format!("  {}", snap.title), 0, sbase, FG_DIM, w as i32 - right_margin);
        }

        // ── Context menu ──────────────────────────────────────────────────
        if let Some((cmx, cmy, items, hovered)) = &ctx_menu_snap {
            let item_h  = lh + 2;
            let sep_h   = 5i32;
            let label_w = items.iter().filter(|i| i.action != CtxAction::Separator).map(|i| i.label.chars().count()).max().unwrap_or(8) as i32;
            let sc_w    = items.iter().filter(|i| i.action != CtxAction::Separator).map(|i| key_label(i.shortcut).chars().count()).max().unwrap_or(0) as i32;
            let menu_w  = (label_w + sc_w + 4) * cw + 16;
            let total_h: i32 = items.iter().map(|i| if i.action == CtxAction::Separator { sep_h } else { item_h }).sum::<i32>() + 4;
            let menu_x  = (*cmx).min(w as i32 - menu_w).max(0);
            let menu_y  = (*cmy).min(h as i32 - total_h).max(0);
            fill(buf, w, h, menu_x, menu_y, menu_w, total_h, BG2);
            fill(buf, w, h, menu_x, menu_y, menu_w, 1, BORDER);
            fill(buf, w, h, menu_x, menu_y + total_h - 1, menu_w, 1, BORDER);
            fill(buf, w, h, menu_x, menu_y, 1, total_h, BORDER);
            fill(buf, w, h, menu_x + menu_w - 1, menu_y, 1, total_h, BORDER);
            let mut iy = menu_y + 2;
            for (i, item) in items.iter().enumerate() {
                if item.action == CtxAction::Separator {
                    fill(buf, w, h, menu_x + 1, iy + 2, menu_w - 2, 1, BORDER);
                    iy += sep_h;
                    continue;
                }
                if i == *hovered && item.enabled {
                    fill(buf, w, h, menu_x + 1, iy, menu_w - 2, item_h, SEL_BG);
                }
                let label_color = if item.enabled { FG } else { FG_DIM };
                draw_str(buf, w, h, g, item.label, menu_x + cw, iy + asc, label_color, menu_x + menu_w - sc_w * cw - cw - 4);
                if !item.shortcut.is_empty() {
                    let shortcut = key_label(item.shortcut);
                    let sc_x = menu_x + menu_w - shortcut.chars().count() as i32 * cw - cw;
                    draw_str(buf, w, h, g, &shortcut, sc_x, iy + asc, FG_DIM, menu_x + menu_w - 1);
                }
                iy += item_h;
            }
        }

        // ── Quick finder / command palette overlay (unified) ─────────────
        if qf_open {
            darken_buffer(buf, w, h);
            let item_count = if qf_is_cmd_mode {
                qf_cmd_items.len()
            } else if qf_is_goto_mode {
                1  // one placeholder row
            } else {
                qf_items.len().max(if qf_direct_item.is_some() { 1 } else { 0 }).max(if qf_loading { 1 } else { 0 })
            };
            let ow = (w as i32 * 2 / 3).min(w as i32 - 40).max(360);
            let oh = lh * (item_count as i32 + 2) + 8;
            let ox = (w as i32 - ow) / 2;
            let oy = h as i32 / 4;
            fill(buf, w, h, ox, oy, ow, oh, BG2);
            fill(buf, w, h, ox, oy, ow, 1, BORDER);
            fill(buf, w, h, ox, oy + oh - 1, ow, 1, BORDER);
            fill(buf, w, h, ox, oy, 1, oh, BORDER);
            fill(buf, w, h, ox + ow - 1, oy, 1, oh, BORDER);
            // Query row
            if qf_is_cmd_mode {
                // Selection highlight
                if let Some(anc) = qf_sel_anchor_chars {
                    let mn = anc.min(qf_cursor_chars);
                    let mx = anc.max(qf_cursor_chars);
                    if mn < mx {
                        let x0 = ox + 4 + mn as i32 * cw;
                        let x1 = (ox + 4 + mx as i32 * cw).min(ox + ow - 4);
                        fill(buf, w, h, x0, oy + 4, (x1 - x0).max(0), lh, HL_MATCH);
                    }
                }
                // Leading '>' in accent, rest of query in FG
                draw_str(buf, w, h, g, ">", ox + 4, oy + 4 + asc, ACCENT, ox + 4 + cw);
                if qf_query.len() > 1 {
                    draw_str(buf, w, h, g, &qf_query[1..], ox + 4 + cw, oy + 4 + asc, FG, ox + ow - 4);
                }
                let cur_x = ox + 4 + qf_cursor_chars as i32 * cw;
                fill(buf, w, h, cur_x.min(ox + ow - 4), oy + 4, 1, lh, ACCENT);
            } else if qf_is_goto_mode {
                // Selection highlight
                if let Some(anc) = qf_sel_anchor_chars {
                    let mn = anc.min(qf_cursor_chars);
                    let mx = anc.max(qf_cursor_chars);
                    if mn < mx {
                        let x0 = ox + 4 + mn as i32 * cw;
                        let x1 = (ox + 4 + mx as i32 * cw).min(ox + ow - 4);
                        fill(buf, w, h, x0, oy + 4, (x1 - x0).max(0), lh, HL_MATCH);
                    }
                }
                // Leading ':' in accent, rest of query in FG
                draw_str(buf, w, h, g, ":", ox + 4, oy + 4 + asc, ACCENT, ox + 4 + cw);
                if qf_query.len() > 1 {
                    draw_str(buf, w, h, g, &qf_query[1..], ox + 4 + cw, oy + 4 + asc, FG, ox + ow - 4);
                }
                let cur_x = ox + 4 + qf_cursor_chars as i32 * cw;
                fill(buf, w, h, cur_x.min(ox + ow - 4), oy + 4, 1, lh, ACCENT);
            } else {
                // Selection highlight
                if let Some(anc) = qf_sel_anchor_chars {
                    let mn = anc.min(qf_cursor_chars);
                    let mx = anc.max(qf_cursor_chars);
                    if mn < mx {
                        let x0 = ox + 4 + 2 * cw + mn as i32 * cw;
                        let x1 = (ox + 4 + 2 * cw + mx as i32 * cw).min(ox + ow - 4);
                        fill(buf, w, h, x0, oy + 4, (x1 - x0).max(0), lh, HL_MATCH);
                    }
                }
                draw_str(buf, w, h, g, "> ", ox + 4, oy + 4 + asc, FG_DIM, ox + ow - 4);
                draw_str(buf, w, h, g, &qf_query, ox + 4 + 2 * cw, oy + 4 + asc, FG, ox + ow - 4);
                let cur_x = ox + 4 + 2 * cw + qf_cursor_chars as i32 * cw;
                fill(buf, w, h, cur_x.min(ox + ow - 4), oy + 4, 1, lh, ACCENT);
            }
            // Result rows
            if qf_is_cmd_mode {
                for (i, (name, shortcut)) in qf_cmd_items.iter().enumerate() {
                    let ry = oy + 4 + lh + 4 + i as i32 * lh;
                    if i == qf_cmd_sel_in_view { fill(buf, w, h, ox + 1, ry, ow - 2, lh, SEL_BG); }
                    draw_str(buf, w, h, g, name, ox + 6, ry + asc, FG, ox + ow - 4);
                    if !shortcut.is_empty() {
                        let sw = shortcut.chars().count() as i32 * cw;
                        draw_str(buf, w, h, g, shortcut, ox + ow - 4 - sw, ry + asc, FG_DIM, ox + ow - 2);
                    }
                }
            } else if qf_is_goto_mode {
                let ry = oy + 4 + lh + 4;
                draw_str(buf, w, h, g, "Go to line number [:col number]", ox + 6, ry + asc, FG_DIM, ox + ow - 4);
            } else if qf_loading {
                let ry = oy + 4 + lh + 4;
                draw_str(buf, w, h, g, "Searching files...", ox + 6, ry + asc, FG_DIM, ox + ow - 4);
            } else {
                let avail_chars = ((ow - 10) / cw).max(4) as usize;
                if let Some((ref name, ref dir)) = qf_direct_item {
                    // Show direct absolute/~ path match when no fuzzy results
                    let ry = oy + 4 + lh + 4;
                    fill(buf, w, h, ox + 1, ry, ow - 2, lh, SEL_BG);
                    let dir_max = (avail_chars / 3).max(1);
                    let dir_disp = fit_str(dir, dir_max);
                    let name_max = avail_chars.saturating_sub(dir_disp.chars().count() + 2);
                    let name_disp = fit_str(name, name_max);
                    let dir_w = dir_disp.chars().count() as i32 * cw;
                    draw_str(buf, w, h, g, &name_disp, ox + 6, ry + asc, FG, ox + ow - 4 - dir_w - cw);
                    draw_str(buf, w, h, g, &dir_disp, ox + ow - 4 - dir_w, ry + asc, FG_DIM, ox + ow - 2);
                } else {
                    for (i, (name, dir)) in qf_items.iter().enumerate() {
                        let ry = oy + 4 + lh + 4 + i as i32 * lh;
                        if i == qf_sel_in_view { fill(buf, w, h, ox + 1, ry, ow - 2, lh, SEL_BG); }
                        let dir_max = (avail_chars / 3).max(1);
                        let dir_disp = fit_str(dir, dir_max);
                        let name_max = avail_chars.saturating_sub(dir_disp.chars().count() + 2);
                        let name_disp = fit_str(name, name_max);
                        let dir_w = dir_disp.chars().count() as i32 * cw;
                        draw_str(buf, w, h, g, &name_disp, ox + 6, ry + asc, FG, ox + ow - 4 - dir_w - cw);
                        draw_str(buf, w, h, g, &dir_disp, ox + ow - 4 - dir_w, ry + asc, FG_DIM, ox + ow - 2);
                    }
                }
            }
        }

        // ── LSP manager panel overlay ────────────────────────────────────
        if let Some((rows, sel)) = &lsp_panel_snap {
            darken_buffer(buf, w, h);
            let header = "LSP Servers   ↵/g go to window   m move here   x stop   esc close";
            let item_count = (rows.len().max(1)) as i32;
            let ow = (w as i32 * 3 / 4).min(w as i32 - 40).max(420);
            let oh = lh * (item_count + 2) + 8;
            let ox = (w as i32 - ow) / 2;
            let oy = h as i32 / 4;
            fill(buf, w, h, ox, oy, ow, oh, BG2);
            fill(buf, w, h, ox, oy, ow, 1, BORDER);
            fill(buf, w, h, ox, oy + oh - 1, ow, 1, BORDER);
            fill(buf, w, h, ox, oy, 1, oh, BORDER);
            fill(buf, w, h, ox + ow - 1, oy, 1, oh, BORDER);
            draw_str(buf, w, h, g, header, ox + 6, oy + 4 + asc, FG_DIM, ox + ow - 4);
            if rows.is_empty() {
                let ry = oy + 4 + lh + 4;
                draw_str(buf, w, h, g, "No language servers running.", ox + 6, ry + asc, FG_DIM, ox + ow - 4);
            } else {
                let avail = ((ow - 12) / cw).max(8) as usize;
                for (i, row) in rows.iter().enumerate() {
                    let ry = oy + 4 + lh + 4 + i as i32 * lh;
                    if i == *sel { fill(buf, w, h, ox + 1, ry, ow - 2, lh, SEL_BG); }
                    let disp = fit_str(row, avail);
                    draw_str(buf, w, h, g, &disp, ox + 6, ry + asc, FG, ox + ow - 4);
                }
            }
        }

        // ── Cmd+hover underline ──────────────────────────────────────────
        if let Some((x1, py_top, x2, _py_bot)) = cmd_hover_underline {
            // Underline at 1px above bottom of line (between baseline and descenders)
            fill(buf, w, h, x1, py_top + lh - 2, x2 - x1, 1, ACCENT);
        }

        // ── Hover tooltip (diagnostic message) ───────────────────────────
        if let Some((tx, ty, ref msg)) = hover_tip {
            let max_chars = (w as i32 / cw - 4).max(10) as usize;
            let disp: String = msg.chars().take(max_chars).collect();
            let tip_w  = disp.chars().count() as i32 * cw + 8;
            let tip_h  = lh + 4;
            let tip_x  = tx.min(w as i32 - tip_w - 4).max(0);
            let tip_y  = (ty - tip_h).max(0);
            fill(buf, w, h, tip_x, tip_y, tip_w, tip_h, BG2);
            fill(buf, w, h, tip_x, tip_y, tip_w, 1, BORDER);
            fill(buf, w, h, tip_x, tip_y + tip_h - 1, tip_w, 1, BORDER);
            fill(buf, w, h, tip_x, tip_y, 1, tip_h, BORDER);
            fill(buf, w, h, tip_x + tip_w - 1, tip_y, 1, tip_h, BORDER);
            draw_str(buf, w, h, g, &disp, tip_x + 4, tip_y + 2 + asc, FG, tip_x + tip_w - 1);
        }
    });
}

// ── Entry point ───────────────────────────────────────────────────────────────
fn main() {
    let arg = std::env::args().nth(1);
    let (file_arg, dir_arg) = match arg {
        Some(s) => {
            let vp = VPath::parse(&s);
            // For local paths: check if it's a directory. For remote: always treat as dir arg.
            match &vp {
                VPath::Local(p) if p.is_dir() => {
                    // Canonicalize to an absolute path so the LSP workspace root is a
                    // valid `file:///abs/path` URI. A relative arg like `.` would otherwise
                    // be sent as `rootUri: file://.`, which servers reject.
                    let dir = os::canonicalize(p).map(VPath::Local).unwrap_or(vp);
                    (None, Some(dir))
                }
                VPath::Remote { .. }           => (None, Some(vp)),
                // Files get an absolute path too (LSP URIs, matching explorer paths);
                // a file that doesn't exist yet is made absolute against the cwd.
                VPath::Local(p) => {
                    let abs = os::canonicalize(p).ok()
                        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(p)))
                        .unwrap_or_else(|| p.clone());
                    (Some(VPath::Local(abs)), None)
                }
            }
        }
        None => (None, None),
    };
    let el = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    let proxy = el.create_proxy();
    let mut app = App::new(file_arg, dir_arg, proxy);
    if let Err(e) = el.run_app(&mut app) {
        eprintln!("local-text: event loop exited with error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ch(c: &str) -> Key { Key::Character(c.into()) }

    #[test]
    fn app_shortcuts_pass_through_sidebar_fields_but_field_editing_keys_do_not() {
        // (key, cmd, ctrl, shift)
        let passes = [
            (ch("s"), true, false, false),   // save
            (ch("s"), false, true, false),   // Ctrl+S save
            (ch("w"), true, false, false),   // close tab
            (ch("1"), true, false, false),   // select tab
            (ch("}"), true, false, true),    // Cmd+Shift+] as macOS reports it
            (ch("]"), true, false, true),
            (ch("{"), true, false, true),    // Cmd+Shift+[
            (ch("G"), true, false, true),    // Cmd+Shift+G find previous
            (ch("\\"), true, false, false),  // split right
            (Key::Named(NamedKey::Tab), false, true, false), // Ctrl+Tab
        ];
        for (key, cmd, ctrl, shift) in passes {
            assert!(is_app_shortcut(&key, cmd, ctrl, shift), "{key:?} cmd={cmd} ctrl={ctrl} shift={shift}");
        }
        let stays = [
            (ch("s"), false, false, false),  // typing an "s"
            (ch("a"), true, false, false),   // select all in the field
            (ch("c"), true, false, false),
            (ch("v"), true, false, false),
            (ch("x"), true, false, false),
            (ch("z"), true, false, false),   // must not undo the editor behind the field
            (ch("\\"), true, false, true),
            (Key::Named(NamedKey::Tab), false, false, false), // field focus cycling
            (Key::Named(NamedKey::Enter), true, false, false),
        ];
        for (key, cmd, ctrl, shift) in stays {
            assert!(!is_app_shortcut(&key, cmd, ctrl, shift), "{key:?} cmd={cmd} ctrl={ctrl} shift={shift}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_shortcut_labels_name_ctrl_and_alt() {
        assert_eq!(key_label("Cmd+Shift+F12"), "Ctrl+Shift+F12");
        assert_eq!(key_label("Opt+Shift+F"), "Alt+Shift+F");
        assert_eq!(key_label("Cmd+= / Cmd+-"), "Ctrl+= / Ctrl+-");
        assert_eq!(key_label("Ctrl+`"), "Ctrl+`");
        assert_eq!(key_label("F12"), "F12");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_text_fields_use_ctrl_for_words_and_ignore_alt_letters() {
        // (field, cursor) after one key; args: key, cmd, alt, ctrl, shift.
        // On Linux the key handler passes cmd == ctrl for Ctrl.
        let edit = |text: &str, cursor: usize, key: Key, cmd: bool, alt: bool, ctrl: bool, shift: bool| {
            let (mut f, mut c, mut sel) = (text.to_owned(), cursor, None);
            input_field_edit(&mut f, &mut c, &mut sel, &key, cmd, alt, ctrl, shift);
            (f, c)
        };
        let left  = Key::Named(NamedKey::ArrowLeft);
        let bs    = Key::Named(NamedKey::Backspace);
        // Ctrl+Left: previous word (macOS-style "start of line" was used before)
        assert_eq!(edit("foo bar", 7, left, true, false, true, false), ("foo bar".to_owned(), 4));
        // Ctrl+Backspace: delete the previous word, not the whole field
        assert_eq!(edit("foo bar", 7, bs.clone(), true, false, true, false), ("foo ".to_owned(), 4));
        // Ctrl+Shift+Backspace clears it
        assert_eq!(edit("foo bar", 7, bs, true, false, true, true), (String::new(), 0));
        // Alt+letter is a shortcut, not text
        assert_eq!(edit("foo", 3, ch("x"), false, true, false, false), ("foo".to_owned(), 3));
        assert_eq!(edit("foo", 3, ch("x"), false, false, false, false), ("foox".to_owned(), 4));
    }

    #[test]
    fn modifier_only_presses_are_recognised() {
        for k in [NamedKey::Shift, NamedKey::Super, NamedKey::Control, NamedKey::Alt, NamedKey::AltGraph] {
            assert!(is_modifier_key(&Key::Named(k)));
        }
        assert!(!is_modifier_key(&ch("a")));
        assert!(!is_modifier_key(&Key::Named(NamedKey::ArrowDown)));
    }

    fn test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "local-text-{label}-{}-{nonce}",
            std::process::id(),
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn apply_delta_saturates_and_clamps() {
        assert_eq!(apply_delta(5, 3, 100), 8);
        assert_eq!(apply_delta(5, -10, 100), 0);    // negative sum would wrap to huge usize before
        assert_eq!(apply_delta(5, 1000, 100), 100); // clamped to len, never out of bounds
        assert_eq!(apply_delta(0, -1, 0), 0);
    }

    #[test]
    fn local_save_replaces_contents_without_leftovers() {
        let dir = test_dir("atomic-save-basic");
        let target = dir.join("file with space.txt");
        std::fs::write(&target, "old contents that are longer").unwrap();
        write_file_atomically(&target, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        write_file_atomically(&dir.join("created.txt"), b"fresh").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("created.txt")).unwrap(), "fresh");
        let leftovers = std::fs::read_dir(&dir).unwrap().flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("lt-save")).count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn format_command_quotes_filename_for_cmd() {
        assert_eq!(
            build_format_command("prettier --write {file}", r"C:\My Docs\a&b.ts"),
            r#"prettier --write "C:\My Docs\a&b.ts""#,
        );
        assert_eq!(build_format_command("rustfmt", r"C:\x.rs"), r#"rustfmt "C:\x.rs""#);
    }

    #[test]
    fn fuzzy_path_matching_accepts_either_separator() {
        assert!(fuzzy_score_path("src/main", "src/main.rs").is_some());
        assert_eq!(path_basename("a/b/c.rs"), "c.rs");
        if cfg!(windows) {
            assert!(fuzzy_score_path("src/main", r"src\main.rs").is_some());
            assert_eq!(path_basename(r"a\b\c.rs"), "c.rs");
        }
    }

    #[cfg(unix)]
    #[test]
    fn format_command_shell_quotes_filename() {
        // A maliciously-named file must be quoted, not interpolated raw into `sh -c`.
        assert_eq!(
            build_format_command("prettier --write {file}", "a b;$(rm -rf ~).ts"),
            "prettier --write 'a b;$(rm -rf ~).ts'",
        );
        // No {file} placeholder → quoted path appended.
        assert_eq!(
            build_format_command("rustfmt", "/tmp/x.rs"),
            "rustfmt '/tmp/x.rs'",
        );
        // Embedded single quote is escaped correctly (POSIX '\'' idiom).
        assert_eq!(
            build_format_command("fmt", "/tmp/x'y.rs"),
            "fmt '/tmp/x'\\''y.rs'",
        );
    }

    #[test]
    fn python_is_detected_from_shebang_without_python_extension() {
        assert_eq!(
            Lang::from_path_and_first_line(
                Some(std::path::Path::new("tool")),
                "#!/usr/bin/env -S python3 -u\n",
            ),
            Lang::Python,
        );
        assert_eq!(
            Lang::from_path_and_first_line(
                Some(std::path::Path::new("tool.txt")),
                "#!/opt/homebrew/bin/python3.13",
            ),
            Lang::Python,
        );
        assert_eq!(
            Lang::from_path_and_first_line(
                Some(std::path::Path::new("tool.rs")),
                "#!/usr/bin/python3",
            ),
            Lang::Rust,
        );
        assert_eq!(
            Lang::from_path_and_first_line(None, "#!/bin/sh"),
            Lang::None,
        );
    }

    #[test]
    fn python_block_header_detection_ignores_comments_and_string_hashes() {
        assert!(python_line_opens_block("    if ready:"));
        assert!(python_line_opens_block("if ready:  # explanation"));
        assert!(!python_line_opens_block("value = \"#:\""));
        assert!(!python_line_opens_block("print('not a block')"));
    }

    #[test]
    fn stale_field_cursor_is_clamped_to_a_utf8_boundary() {
        assert_eq!(clamp_byte_index("", 4), 0);
        assert_eq!(clamp_byte_index("é", 1), 0);
        assert_eq!(clamp_byte_index("é", 2), 2);
        assert_eq!(clamp_byte_index("é", usize::MAX), 2);
    }

    #[test]
    fn explorer_refresh_finds_new_files_and_preserves_expansion() {
        let root = test_dir("refresh");
        let nested = root.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let mut explorer = FileExplorer::new(VPath::Local(root.clone()));
        let nested_idx = explorer.entries.iter()
            .position(|entry| entry.path == nested)
            .unwrap();
        explorer.toggle(nested_idx);

        let new_path = nested.join("created.py");
        std::fs::write(&new_path, "print('ok')\n").unwrap();
        assert!(explorer.refresh_local());
        assert!(explorer.entries.iter().any(|entry| entry.path == new_path));
        assert!(explorer.entries.iter().any(|entry| {
            entry.path == nested && entry.expanded
        }));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_explorer_refresh_finds_new_files_and_preserves_expansion() {
        let host = vpath::SshHost {
            user: Some("dev".to_owned()),
            host: "example.test".to_owned(),
            port: Some(2222),
        };
        let mut explorer = FileExplorer::new(VPath::Remote {
            host,
            path: PathBuf::from("/workspace"),
        });
        apply_remote_dir_listing(
            &mut explorer,
            Path::new("/workspace"),
            vec![("src".to_owned(), true), ("Cargo.toml".to_owned(), false)],
        );
        let src_idx = explorer.entries.iter()
            .position(|entry| entry.path == Path::new("/workspace/src"))
            .unwrap();
        explorer.toggle(src_idx);
        apply_remote_dir_listing(
            &mut explorer,
            Path::new("/workspace/src"),
            vec![("main.rs".to_owned(), false)],
        );
        explorer.selected = explorer.entries.iter()
            .position(|entry| entry.path == Path::new("/workspace/src/main.rs"))
            .unwrap();

        apply_remote_dir_listing(
            &mut explorer,
            Path::new("/workspace"),
            vec![
                ("src".to_owned(), true),
                ("Cargo.toml".to_owned(), false),
                ("created-from-terminal".to_owned(), false),
            ],
        );

        assert!(explorer.entries.iter().any(|entry| {
            entry.path == Path::new("/workspace/created-from-terminal")
        }));
        assert!(explorer.entries.iter().any(|entry| {
            entry.path == Path::new("/workspace/src") && entry.expanded
        }));
        assert_eq!(
            explorer.entries[explorer.selected].path,
            Path::new("/workspace/src/main.rs"),
        );
    }

    #[test]
    fn recursive_copy_copies_directories_and_rejects_self_copy() {
        let root = test_dir("copy");
        let source = root.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("file.txt"), "content").unwrap();
        let destination = root.join("destination");
        copy_path_recursively(&source, &destination).unwrap();
        assert_eq!(
            std::fs::read_to_string(destination.join("file.txt")).unwrap(),
            "content",
        );
        let other = root.join("other");
        std::fs::create_dir(&other).unwrap();
        assert_eq!(paste_destination(&other, &source), other.join("source"));
        std::fs::create_dir(other.join("source")).unwrap();
        assert_eq!(paste_destination(&other, &source), other.join("source copy"));
        assert!(copy_path_recursively(&source, &source.join("nested-copy")).is_err());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_diff_tabs_can_never_be_saved() {
        let dir = test_dir("diff-save");
        std::fs::write(dir.join("main.rs"), "fn main() { /* uncommitted */ }\n").unwrap();
        let mut tab = Tab::untitled(1);
        tab.kind = TabKind::GitDiff;
        tab.path = Some(VPath::Local(dir.join("main.rs")));
        assert!(tab.save_with_proxy(None).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("main.rs")).unwrap(), "fn main() { /* uncommitted */ }\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn local_save_is_atomic_and_keeps_links_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_dir("atomic-save");
        let target = dir.join("real.txt");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_file_atomically(&link, b"new contents").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new contents");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        let leftovers = std::fs::read_dir(&dir).unwrap().flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("lt-save")).count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replace_in_text_keeps_line_endings_and_final_newline() {
        let (out, n) = replace_in_text("foo one\r\nbar two\r\nfoo three\r\n", "foo", "baz", true).unwrap();
        assert_eq!(out, "baz one\r\nbar two\r\nbaz three\r\n");
        assert_eq!(n, 2);
        let (out, _) = replace_in_text("a FOO\nno newline foo", "foo", "x", false).unwrap();
        assert_eq!(out, "a x\nno newline x");
        assert!(replace_in_text("nothing here\n", "foo", "x", true).is_none());
    }

    #[test]
    fn find_offsets_survive_case_folding_that_changes_length() {
        // 'İ' lowercases to two chars; matches after it must still map to the
        // right original positions.
        let text = Rope::from_str("İİ foo Foo\nfoo");
        assert_eq!(find_matches(&text, "foo", false, false), vec![(3, 6), (7, 10), (11, 14)]);
        assert_eq!(find_matches(&text, "foo", true, true), vec![(3, 6), (11, 14)]);
        // Non-overlapping, and a match can't start inside a folded char.
        assert_eq!(find_matches(&Rope::from_str("aaaa"), "aa", true, false), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn lsp_utf16_positions_convert_and_clamp() {
        let text = Rope::from_str("a😀b\nxy");
        assert_eq!(utf16_to_char_col(&text, 0, 3), 2); // after the emoji (2 UTF-16 units)
        assert_eq!(char_col_to_utf16(&text, 0, 2), 3);
        assert_eq!(utf16_to_char_col(&text, 0, 99), 3); // past end of line → end of line
        assert_eq!(lsp_position_to_char(&text, 7, 0), text.len_chars()); // past last line
        assert_eq!(char_to_lsp_position(&text, 3), (0, 4));
    }

    #[test]
    fn lsp_edits_apply_in_order_without_panicking() {
        let mut text = Rope::from_str("a😀b\nc");
        let edits = serde_json::json!([
            { "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 4 } }, "newText": "B" },
            { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "1" },
            { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "2" },
            // Out of range / stale edits are clamped instead of crashing.
            { "range": { "start": { "line": 9, "character": 0 }, "end": { "line": 99, "character": 5 } }, "newText": "!" }
        ]);
        let ranges = lsp_edits_to_char_ranges(&text, edits.as_array().unwrap());
        let mut cursors = vec![Cursor::new(4)];
        assert!(apply_char_edits(&mut text, &mut cursors, ranges));
        assert_eq!(text.to_string(), "12a😀B\nc!");
        assert_eq!(cursors[0].head, 6);
    }

    #[test]
    fn json_formatting_preserves_comments_numbers_and_trailing_commas() {
        let src = "{\n  // leading comment\n  \"big\": 12345678901234567890123, \"f\": 1.50,\n  \"arr\": [1, 2, /* inline */ 3,],\n  \"empty\": {}, \"url\": \"http://x//y\" // trailing\n}";
        let out = format_json_text(src, true).unwrap();
        assert!(out.contains("// leading comment"));
        assert!(out.contains("/* inline */"));
        assert!(out.contains("// trailing"));
        assert!(out.contains("12345678901234567890123"));
        assert!(out.contains("1.50"));
        assert!(out.contains("\"http://x//y\""));
        assert!(out.contains("\"empty\": {}"));
        assert!(out.contains("3,\n  ]"));
        // Strict JSON with comments is rejected (unchanged), JSONC is fine.
        assert!(format_json_text("{\"a\": 1 // c\n}", false).is_none());
        assert!(format_json_text("{\"a\": [1,2]}", false).unwrap().starts_with("{\n  \"a\": [\n    1,\n    2\n  ]\n}"));
        assert!(format_json_text("{\"a\": ", true).is_none());
        // Formatting is idempotent.
        assert_eq!(format_json_text(&out, true).unwrap(), out);
    }

    #[test]
    fn porcelain_z_parsing_handles_spaces_renames_and_untracked() {
        let out = "M  src/a file.rs\0R  new name.rs\0old name.rs\0?? untracked é.txt\0 M src/b.rs\0";
        let (staged, unstaged) = parse_porcelain_z(out);
        let s: Vec<_> = staged.iter().map(|e| e.path.as_str()).collect();
        let u: Vec<_> = unstaged.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(s, ["src/a file.rs", "new name.rs"]);
        assert_eq!(u, ["untracked é.txt", "src/b.rs"]);
    }

    /// Random edits (including multi-byte text) checked against brute force: the
    /// fast chunk diff must find the true changed region, and the collab operation
    /// derived from it must reproduce the edit exactly.
    #[test]
    fn rope_diff_matches_brute_force() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let alphabet = ['a', 'b', '\n', 'é', '😀', ' ', 'x'];
        let mut base = String::new();
        for _ in 0..20_000 { base.push(alphabet[rng.gen_range(0..alphabet.len())]); }
        let mut before = Rope::from_str(&base);
        for _ in 0..200 {
            let mut after = before.clone();
            let len = after.len_chars();
            let start = rng.gen_range(0..=len);
            let del = rng.gen_range(0..=(len - start).min(30));
            after.remove(start..start + del);
            let ins: String = (0..rng.gen_range(0..10)).map(|_| alphabet[rng.gen_range(0..alphabet.len())]).collect();
            after.insert(start, &ins);

            let op = diff_to_operation(&before, &after);
            if before == after {
                assert!(op.is_none() && changed_line_range(&before, &after).is_none());
            } else {
                let mut applied = before.clone();
                op.unwrap().apply(&mut applied).unwrap();
                assert_eq!(applied, after);
                let (first, last) = changed_line_range(&before, &after).unwrap();
                // Every line outside [first, last] is unchanged (compare with old).
                let (bs, as_) = (before.to_string(), after.to_string());
                let bl: Vec<&str> = bs.split('\n').collect();
                let al: Vec<&str> = as_.split('\n').collect();
                for i in 0..first { assert_eq!(bl[i], al[i]); }
                let tail = al.len() - 1 - last;
                for k in 0..tail { assert_eq!(bl[bl.len() - 1 - k], al[al.len() - 1 - k]); }
            }
            before = after;
        }
    }
}
