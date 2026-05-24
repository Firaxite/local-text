mod platform;
mod settings;
mod terminal;
mod lsp;
mod vpath;
mod ssh;
mod collab;

use std::collections::HashMap;
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
enum Lang { None, Rust, Python, TypeScript, Json, Jsonc, Markdown }

impl Lang {
    fn from_path(path: &std::path::Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs")                          => Lang::Rust,
            Some("py" | "pyw")                  => Lang::Python,
            Some("ts" | "tsx" | "js" | "jsx")   => Lang::TypeScript,
            Some("json")                        => Lang::Json,
            Some("jsonc")                       => Lang::Jsonc,
            Some("md" | "markdown")             => Lang::Markdown,
            _                                   => Lang::None,
        }
    }
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
}

// ── Layout ────────────────────────────────────────────────────────────────────
const FONT_PX:  f32 = 14.0;
const ED_LPAD:  i32 = 6;
const SB_W:       i32 = 6;
const SB_THUMB:   u32 = 0x414868;

// ── Glyph cache ───────────────────────────────────────────────────────────────

// Fallback font paths tried in order when a character isn't in the primary font.
// Fonts are loaded lazily on first use to avoid paying ~43 MiB of heap at startup
// for font outlines (fontdue parses all glyphs eagerly in Font::from_bytes).
const FALLBACK_PATHS: &[&str] = &[
    "/System/Library/Fonts/Apple Symbols.ttf",
    "/System/Library/Fonts/Symbol.ttf",
    "/System/Library/Fonts/Supplemental/STIXTwoMath.otf",
];

struct Glyphs {
    font:       Font,
    fallbacks:  Vec<Option<Font>>,  // None = not yet loaded; indexed parallel to FALLBACK_PATHS
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
        let mut s = Self { font, fallbacks: vec![None; FALLBACK_PATHS.len()],
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
        // Try fallback fonts; load lazily on first use
        for i in 0..FALLBACK_PATHS.len() {
            if self.fallbacks[i].is_none() {
                self.fallbacks[i] = std::fs::read(FALLBACK_PATHS[i]).ok()
                    .and_then(|b| Font::from_bytes(b, FontSettings::default()).ok());
            }
            let has_glyph = self.fallbacks[i].as_ref()
                .map(|f| f.lookup_glyph_index(ch) != 0)
                .unwrap_or(false);
            if has_glyph {
                let (m, b) = self.fallbacks[i].as_ref().unwrap().rasterize(ch, self.px);
                self.map.insert(ch, (m, b));
                self.evict_if_over_cap();
                return;
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
    fn new(pos: usize) -> Self { Cursor { head: pos, tail: pos } }
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
            open: false, replace_open: false,
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
enum LeftView { FileTree, GlobalSearch, Diagnostics, Git }

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
    loading:        bool,
    scroll:         usize,
}

impl GitPanel {
    fn new() -> Self {
        GitPanel {
            staged: vec![], unstaged: vec![],
            commit_msg: String::new(), commit_cursor: 0,
            commit_focused: false, sel: GitSel::None,
            is_git_repo: true, loading: false, scroll: 0,
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
    TabCopyRelPath, TabCopyFullPath,
    TabOpenPreview,
    TabClose,
    TabSplitRight, TabSplitLeft, TabSplitDown, TabSplitUp,
    GitOpenFile, GitViewDiff, GitStage, GitUnstage,
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsFieldId { FormatOnSave, OrganizeImportsOnSave, FormatCommand }

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
}

// ── Quick file finder ─────────────────────────────────────────────────────────
struct QuickFinder {
    open:               bool,
    query:              String,
    cursor:             usize,
    sel_anchor:         Option<usize>,
    entries:            Vec<VPath>,
    filtered:           Vec<usize>,
    filtered_commands:  Vec<usize>,
    selected:           usize,
    restore_tree_focus: bool,
    walk_token:         u64,   // incremented on each open; stale QuickFinderFiles events are dropped
    loading:            bool,  // true while background walk_files is in progress
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
    hl_color_cache: Vec<Vec<u32>>,      // cached highlight color per char, indexed by line
    max_line_len:   Option<usize>, // cached max line length (chars), None = needs recompute
    edit_generation: u64, // incremented on every text mutation; used to skip redundant syncs
}

impl Tab {
    fn untitled(buf_id: usize) -> Self {
        Tab { kind: TabKind::Editor, buf_id, text: Rope::new(), path: None, dirty: false,
              cursors: vec![Cursor::new(0)], scroll: 0, hscroll: 0,
              undo_stack: Vec::new(), redo_stack: Vec::new(), last_typing: false,
              hl_cache: Vec::new(), hl_dirty_from: 0, hl_color_cache: Vec::new(),
              max_line_len: Some(0), edit_generation: 0 }
    }

    fn settings() -> Self {
        Tab { kind: TabKind::Settings, buf_id: usize::MAX, text: Rope::new(),
              path: None, dirty: false, cursors: vec![Cursor::new(0)],
              scroll: 0, hscroll: 0,
              undo_stack: Vec::new(), redo_stack: Vec::new(), last_typing: false,
              hl_cache: Vec::new(), hl_dirty_from: 0, hl_color_cache: Vec::new(),
              max_line_len: Some(0), edit_generation: 0 }
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
                .unwrap_or("Untitled"),
        }
    }

    fn is_empty_untitled(&self) -> bool {
        self.kind == TabKind::Editor && self.path.is_none() && self.text.len_chars() == 0
    }

    fn primary(&self) -> &Cursor {
        debug_assert!(!self.cursors.is_empty(), "cursors must never be empty");
        self.cursors.last().unwrap()
    }
    fn primary_mut(&mut self) -> &mut Cursor {
        if self.cursors.is_empty() { self.cursors.push(Cursor::new(0)); }
        self.cursors.last_mut().unwrap()
    }

    fn sel(&self) -> Option<(usize, usize)> { self.primary().sel() }

    fn sel_text(&self) -> Option<String> {
        self.sel().map(|(lo, hi)| self.text.slice(lo..hi).chars().collect())
    }

    fn load_file(&mut self, path: VPath) -> bool {
        // Remote files are loaded asynchronously (Phase 3); for now only local works.
        let Some(local_path) = path.as_local_path() else {
            self.path = Some(path);
            return false;
        };
        const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
        if std::fs::metadata(local_path).map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
            return false;
        }
        match std::fs::read_to_string(local_path) {
            Ok(content) => {
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
                self.max_line_len = None;
                self.edit_generation += 1;
                true
            }
            Err(e) => { eprintln!("open error: {e}"); false }
        }
    }

    fn save(&mut self) {
        self.save_with_proxy(None);
    }

    fn save_with_proxy(&mut self, proxy: Option<&winit::event_loop::EventLoopProxy<UserEvent>>) {
        let Some(path) = &self.path else { return };
        let content: String = self.text.chunks().collect();
        match path {
            VPath::Local(local) => match std::fs::write(local, content) {
                Ok(_)  => self.dirty = false,
                Err(e) => eprintln!("save error: {e}"),
            },
            VPath::Remote { host, path: remote_path } => {
                if let Some(proxy) = proxy {
                    ssh::ssh_write_file(host.clone(), remote_path.clone(), content, proxy.clone());
                    // dirty will be cleared on RemoteWriteDone
                } else {
                    eprintln!("save: remote save requires proxy (use save_with_proxy)");
                }
            }
        }
    }
}

// ── File explorer ─────────────────────────────────────────────────────────────
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
    tree_search:             String,
    tree_search_cursor:      usize,
    tree_search_sel_anchor:  Option<usize>,
    tree_search_focused:     bool,
    tree_search_fuzzy:       bool,
    tree_search_entries:     Vec<std::path::PathBuf>,
    tree_search_results:     Vec<std::path::PathBuf>,
    tree_search_sel:         usize,
}

impl FileExplorer {
    fn new(root: VPath) -> Self {
        // Remote directory listing is async (Phase 3); start empty for remote roots.
        let entries = root.as_local_path()
            .map(|p| load_dir_entries(p, 0, false))
            .unwrap_or_default();
        FileExplorer {
            root, entries, selected: 0, show_hidden: false,
            tree_search: String::new(), tree_search_cursor: 0, tree_search_sel_anchor: None,
            tree_search_focused: false, tree_search_fuzzy: true,
            tree_search_entries: vec![], tree_search_results: vec![], tree_search_sel: 0,
        }
    }

    fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        if let Some(local) = self.root.as_local_path() {
            self.entries = load_dir_entries(local, 0, self.show_hidden);
        }
        self.selected = 0;
    }

    fn toggle(&mut self, idx: usize) {
        if !self.entries[idx].is_dir { return; }
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
            // Remote directory expansion is async (Phase 3); skip for now.
            if self.root.as_local_path().is_some() {
                let children = load_dir_entries(&path, depth, self.show_hidden);
                for (i, child) in children.into_iter().enumerate() {
                    self.entries.insert(idx + 1 + i, child);
                }
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
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
enum PaneKind { Editor, Terminal, LspOutput, MarkdownPreview }

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
        _ => {}
    }
}

fn resize_terminal_panes(s: &mut State) {
    let area = s.pane_area();
    let tab_h = s.tab_h();
    let lh    = s.glyphs.lh;
    let cw    = s.glyphs.cw;
    let layout = layout_tree(&s.pane_tree, area);
    for (pid, rect) in layout {
        if s.panes.get(&pid).map_or(false, |p| p.kind == PaneKind::Terminal) {
            let cols = (rect.w / cw).max(1) as usize;
            let rows = ((rect.h - tab_h) / lh).max(1) as usize;
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
    s.explorer.as_ref().and_then(|e| e.root.ssh_host().cloned())
}

fn open_terminal_pane(s: &mut State) {
    let pane_id = s.next_pane_id; s.next_pane_id += 1;
    let term_id = s.next_pane_id; s.next_pane_id += 1;
    let area = s.pane_area();
    let cols = (area.w / s.glyphs.cw).max(1) as usize;
    let rows = ((area.h / 2) / s.glyphs.lh).max(1) as usize;
    let proxy = s.proxy.clone();
    // If we're in a remote workspace, open an SSH session instead of a local shell.
    let tp = if let Some(host) = active_workspace_ssh_host(s) {
        let ssh_cmd = format!(
            "ssh -o ControlPath={} {}",
            host.control_path().display(),
            host.host_arg(),
        );
        terminal::spawn_terminal_with_shell(term_id, cols, rows, proxy, Some(ssh_cmd))
    } else {
        terminal::spawn_terminal(term_id, cols, rows, proxy)
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

fn check_lsp_binaries(s: &mut State) {
    for (lang, bin) in &[(Lang::TypeScript, "typescript-language-server"), (Lang::Rust, "rust-analyzer"), (Lang::Python, "pylsp")] {
        let installed = std::process::Command::new("which").arg(bin).output()
            .map(|o| o.status.success()).unwrap_or(false);
        s.lsp_installed.insert(*lang, installed);
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

pub struct OutputPane {
    pub id:     usize,
    pub lines:  Vec<String>,
    pub scroll: usize,
    pub title:  String,
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
    FormatterDone  { path: VPath },
    GitStatusResult { staged: Vec<GitEntry>, unstaged: Vec<GitEntry>, is_git_repo: bool },
    GitDiffResult   { buf_id: usize, path: String, lines: Vec<DiffLine> },
    GitOpDone,
    SearchDone        { token: u64, results: Vec<GlobalFindResult> },
    QuickFinderFiles  { token: u64, entries: Vec<VPath> },
    Redraw,
    // SSH / remote events
    SshConnecting       { host: vpath::SshHost },
    SshConnected        { host: vpath::SshHost },
    SshError            { host: vpath::SshHost, msg: String },
    RemoteFileContent   { token: u64, path: VPath, content: String },
    RemoteWriteDone     { path: VPath },
    RemoteDirListing    { path: VPath, entries: Vec<(String, bool)> },
    // Collab events
    CollabMessage     { from_site_id: u64, msg: collab::CollabMsg },
    CollabConnected   { session: Box<collab::CollabSession>, doc_text: String, peers: Vec<collab::PeerInfo> },
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
    drag:         Option<DragState>,
    drag_pending: Option<(usize, usize, f32, f32)>,
    resize_drag:  Option<ResizeDrag>,

    term_panes:     HashMap<usize, terminal::TermPane>,
    lsp_panes:      HashMap<usize, OutputPane>,
    md_panes:       HashMap<usize, MarkdownPane>,
    lsp:            lsp::LspManager,
    diagnostics:    HashMap<VPath, Vec<Diagnostic>>,
    lsp_installed:  HashMap<Lang, bool>,
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
    diag_panel_sel: usize,
    context_menu: Option<ContextMenu>,
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
    collab_before: Option<ropey::Rope>,
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

fn clipboard_write(text: &str) {
    use std::io::Write;
    if let Ok(mut child) = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped()).spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() { let _ = stdin.write_all(text.as_bytes()); }
        let _ = child.wait();
    }
}

fn clipboard_read() -> String {
    std::process::Command::new("pbpaste").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

impl State {
    fn pane(&self)         -> &Pane     { &self.panes[&self.active_pane] }
    fn pane_mut(&mut self) -> &mut Pane { let id = self.active_pane; self.panes.get_mut(&id).unwrap() }
    fn tab(&self)          -> &Tab      { self.pane().tab() }
    fn tab_mut(&mut self)  -> &mut Tab  { let id = self.active_pane; self.panes.get_mut(&id).unwrap().tab_mut() }
    fn find(&self)         -> &FindBar  { &self.pane().find }
    fn find_mut(&mut self) -> &mut FindBar { let id = self.active_pane; &mut self.panes.get_mut(&id).unwrap().find }

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
                self.collab_before = Some(self.tab().text.clone());
            }
            let tab = self.tab_mut();
            tab.max_line_len = None;
            tab.hl_dirty_from = 0;
            tab.hl_color_cache.clear();
            tab.edit_generation += 1;
            tab.last_typing = coalesce;
            return;
        }
        let limit = self.settings.undo_limit;
        let tab = self.tab_mut();
        tab.max_line_len = None;
        tab.hl_dirty_from = 0;
        tab.hl_color_cache.clear();
        tab.edit_generation += 1;
        if !coalesce || !tab.last_typing {
            if let Some(lim) = limit {
                if tab.undo_stack.len() >= lim { tab.undo_stack.remove(0); }
            }
            tab.undo_stack.push(UndoEntry { text: tab.text.clone(), cursors: tab.cursors.clone() });
            tab.redo_stack.clear();
        }
        tab.last_typing = coalesce;
    }

    fn undo(&mut self) {
        let limit = self.settings.undo_limit;
        let Some(entry) = self.tab_mut().undo_stack.pop() else { return };
        let cur = UndoEntry { text: self.tab().text.clone(), cursors: self.tab().cursors.clone() };
        let tab = self.tab_mut();
        if let Some(lim) = limit { if tab.redo_stack.len() >= lim { tab.redo_stack.remove(0); } }
        tab.redo_stack.push(cur);
        tab.text    = entry.text;
        tab.cursors = entry.cursors;
        tab.dirty   = true;
        tab.last_typing = false;
        tab.max_line_len = None;
        tab.hl_dirty_from = 0;
        tab.hl_color_cache.clear();
        tab.edit_generation += 1;
        self.ensure_visible();
    }

    fn redo(&mut self) {
        let limit = self.settings.undo_limit;
        let Some(entry) = self.tab_mut().redo_stack.pop() else { return };
        let cur = UndoEntry { text: self.tab().text.clone(), cursors: self.tab().cursors.clone() };
        let tab = self.tab_mut();
        if let Some(lim) = limit { if tab.undo_stack.len() >= lim { tab.undo_stack.remove(0); } }
        tab.undo_stack.push(cur);
        tab.text    = entry.text;
        tab.cursors = entry.cursors;
        tab.dirty   = true;
        tab.last_typing = false;
        tab.max_line_len = None;
        tab.hl_dirty_from = 0;
        tab.hl_color_cache.clear();
        tab.edit_generation += 1;
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
            let lo = (orig_lo as isize + delta) as usize;
            let hi = (orig_hi as isize + delta) as usize;
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
            let lo = (orig_lo as isize + delta) as usize;
            let hi = (orig_hi as isize + delta) as usize;
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

    fn backspace(&mut self) {
        self.push_undo(false);
        let order = self.cursor_order_ltr();
        let mut delta: isize = 0;
        for &i in &order {
            let (has_sel, orig_lo, orig_hi, orig_head) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi(), c.head)
            };
            let lo   = (orig_lo   as isize + delta) as usize;
            let hi   = (orig_hi   as isize + delta) as usize;
            let head = (orig_head as isize + delta) as usize;
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
            let lo   = (orig_lo   as isize + delta) as usize;
            let hi   = (orig_hi   as isize + delta) as usize;
            let head = (orig_head as isize + delta) as usize;
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
            let lo   = (orig_lo   as isize + delta) as usize;
            let hi   = (orig_hi   as isize + delta) as usize;
            let head = (orig_head as isize + delta) as usize;
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
            let lo   = (orig_lo   as isize + delta) as usize;
            let hi   = (orig_hi   as isize + delta) as usize;
            let head = (orig_head as isize + delta) as usize;
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
            let lo   = (orig_lo   as isize + delta) as usize;
            let hi   = (orig_hi   as isize + delta) as usize;
            let head = (orig_head as isize + delta) as usize;
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
            let lo   = (orig_lo   as isize + delta) as usize;
            let hi   = (orig_hi   as isize + delta) as usize;
            let head = (orig_head as isize + delta) as usize;
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
fn open_or_reuse_tab(s: &mut State, path: VPath) {
    // Find the nearest editor pane to open the file in (active if it's an editor,
    // otherwise find the first editor pane in the layout).
    let ap = if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Editor) {
        s.active_pane
    } else {
        let area = s.pane_area();
        let Some(id) = layout_tree(&s.pane_tree, area).into_iter()
            .find(|(id, _)| s.panes.get(id).map_or(false, |p| p.kind == PaneKind::Editor))
            .map(|(id, _)| id)
        else { return; };
        s.active_pane = id;
        id
    };
    let pane = s.panes.get_mut(&ap).unwrap();
    for i in 0..pane.tabs.len() {
        if pane.tabs[i].kind == TabKind::GitDiff { continue; }
        if pane.tabs[i].path.as_ref() == Some(&path) {
            pane.active = i;
            return;
        }
    }
    let loaded = if pane.tab().kind != TabKind::GitDiff && pane.tab().is_empty_untitled() {
        pane.tab_mut().load_file(path.clone())
    } else {
        let mut tab = Tab::untitled(s.next_buf_id);
        s.next_buf_id += 1;
        let ok = tab.load_file(path.clone());
        pane.tabs.push(tab);
        pane.active = pane.tabs.len() - 1;
        ok
    };
    if !loaded {
        match &path {
            VPath::Remote { host, path: remote_path } => {
                // Async load: dispatch SSH read and leave the tab showing empty text.
                let token = s.next_buf_id as u64;  // cheap unique-ish token
                ssh::ssh_read_file(host.clone(), remote_path.clone(), token, s.proxy.clone());
            }
            VPath::Local(_) => {
                // Local file that failed to load (too large).
                s.status_msg = Some(format!("File too large to open (>256 MB): {path}"));
                return;
            }
        }
    }
    // Notify LSP of the opened file
    notify_lsp_open(s, &path);
}

fn apply_goto_definition(s: &mut State, result: &serde_json::Value) {
    let loc = if result.is_array() { result.get(0) } else { Some(result) };
    let Some(loc) = loc else { return };
    let uri  = loc["uri"].as_str().unwrap_or("");
    let line = loc["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
    let col  = loc["range"]["start"]["character"].as_u64().unwrap_or(0) as usize;
    // For now LSP servers are always local; reconstruct as VPath::Local.
    let path = VPath::Local(PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri)));
    push_cursor_history(s);
    open_or_reuse_tab(s, path);
    let pos = s.tab().text.line_to_char(line) + col;
    s.tab_mut().cursors = vec![Cursor::new(pos)];
    s.ensure_visible();
}

fn apply_references(s: &mut State, result: &serde_json::Value) {
    let locs = result.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    s.global_find.results = locs.iter().filter_map(|loc| {
        let uri  = loc["uri"].as_str()?;
        // For now LSP servers are always local; reconstruct as VPath::Local.
        let path = VPath::Local(PathBuf::from(uri.strip_prefix("file://")?));
        let line = loc["range"]["start"]["line"].as_u64()? as usize;
        let col  = loc["range"]["start"]["character"].as_u64()? as usize;
        let text = path.as_local_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| s.lines().nth(line).map(|l| l.to_owned()))
            .unwrap_or_default();
        Some(GlobalFindResult { path, line_num: line, line_text: text, match_col: col, match_len: 1 })
    }).collect();
    s.global_find.selected = 0;
    s.global_find.scroll   = 0;
    s.global_find.focus    = GlobalFindFocus::Results;
    if s.explorer.is_some() { s.left_view = LeftView::GlobalSearch; }
}

fn apply_text_edits(s: &mut State, path: &VPath, result: &serde_json::Value) {
    let edits = result.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    for p in s.panes.values_mut() {
        for t in p.tabs.iter_mut() {
            if t.path.as_ref() != Some(path) { continue; }
            let mut sorted: Vec<(usize, usize, String)> = edits.iter().filter_map(|e| {
                let sl = e["range"]["start"]["line"].as_u64()? as usize;
                let sc = e["range"]["start"]["character"].as_u64()? as usize;
                let el = e["range"]["end"]["line"].as_u64()? as usize;
                let ec = e["range"]["end"]["character"].as_u64()? as usize;
                let new_text = e["newText"].as_str()?.to_owned();
                let start = t.text.line_to_char(sl) + sc;
                let end   = t.text.line_to_char(el) + ec;
                Some((start, end, new_text))
            }).collect();
            sorted.sort_by(|a, b| b.0.cmp(&a.0));
            for (start, end, new_text) in sorted {
                t.text.remove(start..end);
                t.text.insert(start, &new_text);
            }
            t.dirty = true;
            t.hl_dirty_from = 0;
            t.hl_color_cache.clear();
            t.edit_generation += 1;
            return;
        }
    }
}

fn apply_organize_imports(s: &mut State, path: &VPath, result: &serde_json::Value) {
    let actions = match result.as_array() {
        Some(a) => a,
        None    => return,
    };
    let action = actions.iter()
        .find(|a| a["kind"].as_str() == Some("source.organizeImports"))
        .or_else(|| actions.first());
    let Some(action) = action else { return };

    let edit = &action["edit"];
    let uri  = path.to_lsp_uri();

    if let Some(edits) = edit["changes"][&uri].as_array() {
        apply_text_edits(s, path, &serde_json::Value::Array(edits.clone()));
        return;
    }
    if let Some(doc_changes) = edit["documentChanges"].as_array() {
        for dc in doc_changes {
            if dc["textDocument"]["uri"].as_str() == Some(&uri) {
                if let Some(edits) = dc["edits"].as_array() {
                    apply_text_edits(s, path, &serde_json::Value::Array(edits.clone()));
                    return;
                }
            }
        }
    }
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
    (lo, hi)
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

fn open_token(s: &mut State, token: &str) {
    if token.is_empty() { return; }
    if token.starts_with("http://") || token.starts_with("https://") {
        let _ = std::process::Command::new("open").arg(token).spawn();
        return;
    }
    let expanded = if let Some(rest) = token.strip_prefix("~/") {
        std::env::var("HOME").map(|h| format!("{}/{}", h, rest)).unwrap_or_else(|_| token.to_owned())
    } else {
        token.to_owned()
    };
    let path = std::path::Path::new(&expanded);
    if path.exists() {
        open_or_reuse_tab(s, VPath::Local(path.to_path_buf()));
    }
}

fn notify_lsp_open(s: &mut State, path: &VPath) {
    let lang = Lang::from_path(path.as_path());
    if lang == Lang::None { return; }
    // Start server if not running
    if !s.lsp.has_server_for(lang) {
        let op_id = s.next_pane_id;
        s.next_pane_id += 1;
        let proxy = s.proxy.clone();
        let ssh_host = path.ssh_host().cloned();
        if let Some(mut srv) = lsp::start_server(lang, op_id, proxy, ssh_host) {
            // Send initialize request
            let root = path.parent();
            lsp::send_initialize(&mut srv, root.as_ref());
            // Register LSP output pane (not shown in tree until user opens it)
            let title = format!("{:?} LSP Output", lang);
            let op = OutputPane { id: op_id, lines: vec![], scroll: 0, title };
            s.lsp_panes.insert(op_id, op);
            let shell_pane = Pane { id: op_id, kind: PaneKind::LspOutput, tabs: vec![], term_ids: vec![], active: 0, find: FindBar::new() };
            s.panes.insert(op_id, shell_pane);
            s.lsp.servers.insert(op_id, srv);
        }
    }
    // Send didOpen to the server for this language
    let text = {
        let ap = s.active_pane;
        s.panes.get(&ap).and_then(|p| p.tabs.get(p.active)).map(|t| t.text.to_string())
    };
    if let Some(text) = text {
        if let Some(srv) = s.lsp.server_for_lang_mut(lang) {
            lsp::notify_did_open(srv, path, &text);
        }
    }
}

fn notify_lsp_change(s: &mut State) {
    let (path, text, lang) = {
        let ap = s.active_pane;
        let Some(pane) = s.panes.get(&ap) else { return };
        if pane.kind != PaneKind::Editor { return; }
        if pane.tabs.is_empty() { return; }
        let tab = pane.tab();
        let Some(path) = tab.path.clone() else { return };
        let lang = Lang::from_path(path.as_path());
        if lang == Lang::None { return; }
        (path, tab.text.to_string(), lang)
    };
    if let Some(srv) = s.lsp.server_for_lang_mut(lang) {
        lsp::notify_did_change(srv, &path, &text);
    }
}

/// Extract ops from before/after Rope diff and send to collab peers.
/// Called after every keyboard event that might have edited the document.
fn notify_collab_change(s: &mut State) {
    let before = match s.collab_before.take() { Some(b) => b, None => return };
    // Get current text from active editor pane
    let ap = s.active_pane;
    let after = match s.panes.get(&ap).and_then(|p| p.tabs.get(p.active)) {
        Some(tab) if s.panes[&ap].kind == PaneKind::Editor => tab.text.clone(),
        _ => return,
    };
    let site_id = match s.collab.as_ref() { Some(sess) => sess.site_id, None => return };
    let ops = collab::extract_ops(&before, &after, site_id);
    if !ops.is_empty() {
        if let Some(session) = s.collab.as_mut() {
            session.send_local_op(ops);
        }
    }
}

/// Send a debounced CursorUpdate to collab peers if enough time has elapsed.
fn send_collab_cursor_if_needed(s: &mut State) {
    let should_send = s.collab.as_ref()
        .map_or(false, |sess| sess.cursor_debounce_elapsed());
    if !should_send { return; }
    let ap = s.active_pane;
    let cursors: Vec<collab::RemoteCursorPos> = s.panes.get(&ap)
        .filter(|p| p.kind == PaneKind::Editor)
        .map(|p| p.tab().cursors.iter()
            .map(|c| collab::RemoteCursorPos { head: c.head, tail: c.tail })
            .collect())
        .unwrap_or_default();
    if let Some(session) = s.collab.as_mut() {
        session.send_cursor_update(cursors);
    }
}

/// Get the username from the environment for the collab peer name.
fn whoami_or_default() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "Guest".to_owned())
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
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown => false,
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
        Lang::None | Lang::Json | Lang::Jsonc | Lang::Markdown => false,
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

fn format_json_document(s: &mut State) {
    let lang = s.tab().path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);
    let text = s.tab().text.to_string();
    let src = if lang == Lang::Jsonc { strip_jsonc_comments(&text) } else { text };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&src) else { return };
    let Ok(mut pretty) = serde_json::to_string_pretty(&value) else { return };
    pretty.push('\n');
    let t = s.tab_mut();
    let len = t.text.len_chars();
    t.text.remove(0..len);
    t.text.insert(0, &pretty);
    t.dirty = true;
    t.hl_dirty_from = 0;
    t.hl_color_cache.clear();
    t.max_line_len = None;
    t.edit_generation += 1;
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

fn highlight_line(chars: &[char], lang: Lang, mut state: MlState, rainbow: bool, mut bracket_depth: i32) -> (Vec<u32>, MlState, i32) {
    if matches!(lang, Lang::Json | Lang::Jsonc) {
        return highlight_json_line(chars, lang == Lang::Jsonc, state, rainbow, bracket_depth);
    }
    if lang == Lang::Markdown {
        return highlight_markdown_line(chars, state, rainbow, bracket_depth);
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
    { let _ = text; }
}

fn clipboard_get() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        Command::new("pbpaste").output().ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
    }
    #[cfg(not(target_os = "macos"))]
    { None }
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
    #[cfg(not(target_os = "macos"))]
    { None }
}

// ── Find helpers ──────────────────────────────────────────────────────────────

fn find_matches(text: &Rope, query: &str, case_sensitive: bool, whole_word: bool) -> Vec<(usize, usize)> {
    if query.is_empty() { return vec![]; }
    let query_low: String = if case_sensitive { String::new() } else { query.to_lowercase() };
    let qstr = if case_sensitive { query } else { &query_low };
    let qlen = qstr.chars().count();
    if qlen == 0 { return vec![]; }
    let total = text.len_chars();
    let mut out = vec![];
    let mut line_start_char: usize = 0;
    let mut line_buf = String::new();
    let query_chars: Vec<char> = qstr.chars().collect(); // hoist: same for every line
    for line in text.lines() {
        line_buf.clear();
        if case_sensitive {
            line_buf.extend(line.chars().take_while(|&c| c != '\n' && c != '\r'));
        } else {
            for ch in line.chars().take_while(|&c| c != '\n' && c != '\r') {
                for lc in ch.to_lowercase() { line_buf.push(lc); }
            }
        }
        let line_chars: Vec<char> = line_buf.chars().collect();
        let llen = line_chars.len();
        let mut i = 0;
        while i + qlen <= llen {
            if line_chars[i..i + qlen] == query_chars[..] {
                let abs = line_start_char + i;
                let ok = !whole_word || {
                    let before = abs == 0 || !State::is_word_char(text.char(abs - 1));
                    let after  = abs + qlen >= total || !State::is_word_char(text.char(abs + qlen));
                    before && after
                };
                if ok { out.push((abs, abs + qlen)); }
            }
            i += 1;
        }
        line_start_char += line.chars().count();
    }
    out
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
    let (query, cs, ww) = {
        let f = s.find();
        (f.query.clone(), f.case_sensitive, f.whole_word)
    };
    let matches = find_matches(&s.tab().text, &query, cs, ww);
    if matches.is_empty() { return; }
    let repl = s.find().replace.clone();
    for ch in repl.chars() { s.glyphs.load(ch); }
    for &(lo, hi) in matches.iter().rev() {
        s.tab_mut().text.remove(lo..hi);
        s.tab_mut().text.insert(lo, &repl);
    }
    s.tab_mut().dirty = true;
    s.tab_mut().cursors = vec![Cursor::new(0)];
    s.ensure_visible();
}

// ── Quick finder helpers ──────────────────────────────────────────────────────

fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() { return Some(0); }
    let qchars: Vec<char> = query.to_lowercase().chars().collect();
    let cchars: Vec<char> = candidate.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score: i32 = 0;
    let mut last_match: Option<usize> = None;
    for (ci, &c) in cchars.iter().enumerate() {
        if qi < qchars.len() && c == qchars[qi] {
            if let Some(prev) = last_match {
                if ci == prev + 1 { score += 5; } // consecutive bonus
            }
            // word boundary bonus
            if ci == 0 || matches!(cchars[ci - 1], '/' | '_' | '-' | '.') { score += 3; }
            score -= ci as i32 / 4; // earlier match = better
            last_match = Some(ci);
            qi += 1;
        }
    }
    if qi < qchars.len() { None } else { Some(score) }
}

fn fuzzy_score_path(query: &str, path_str: &str) -> Option<i32> {
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
        Some(total)
    } else {
        let filename = path_str.rsplit('/').next().unwrap_or(path_str);
        if let Some(sc) = fuzzy_score(query, filename) {
            Some(sc + 20)
        } else {
            fuzzy_score(query, path_str)
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
            *cursor = if alt { qf_prev_word(field, *cursor) }
                      else if cmd { 0 }
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
            *cursor = if alt { qf_next_word(field, *cursor) }
                      else if cmd { field.len() }
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
                let next = qf_next_char(field, *cursor);
                if next > *cursor { field.drain(*cursor..next); }
            }
            true
        }
        Key::Named(NamedKey::Backspace) => {
            if let Some((mn, mx)) = sel_range {
                field.drain(mn..mx); *cursor = mn; *sel = None;
            } else if cmd {
                field.clear(); *cursor = 0; *sel = None;
            } else if alt {
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
            s if !cmd && !ctrl => {
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

fn walk_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
    if depth > 12 { return; }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') { continue; }
        if name_str == "target" && path.join("CACHEDIR.TAG").exists() { continue; }
        if path.is_dir() { walk_files(&path, out, depth + 1); }
        else if out.len() < 50_000 { out.push(path); }
    }
}

fn open_quick_finder(s: &mut State, proxy: EventLoopProxy<UserEvent>) {
    let had_tree_focus = s.explorer.as_ref().map(|e| e.tree_search_focused).unwrap_or(false);
    // For remote roots, file walking is async via SSH (Phase 3); use empty for now.
    let root = s.explorer.as_ref()
        .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    s.quick_finder.walk_token += 1;
    let token = s.quick_finder.walk_token;
    s.quick_finder.entries             = vec![];
    s.quick_finder.filtered            = vec![];
    s.quick_finder.filtered_commands   = vec![];
    s.quick_finder.query               = String::new();
    s.quick_finder.cursor              = 0;
    s.quick_finder.sel_anchor          = None;
    s.quick_finder.selected            = 0;
    s.quick_finder.restore_tree_focus  = had_tree_focus;
    s.quick_finder.loading             = true;
    s.quick_finder.open                = true;
    // Unfocus tree search and global find so they don't swallow subsequent keystrokes
    if let Some(ex) = s.explorer.as_mut() { ex.tree_search_focused = false; }
    s.global_find.focus = GlobalFindFocus::Results;

    std::thread::spawn(move || {
        let mut local_entries = Vec::new();
        walk_files(&root, &mut local_entries, 0);
        let entries: Vec<VPath> = local_entries.into_iter().map(VPath::Local).collect();
        let _ = proxy.send_event(UserEvent::QuickFinderFiles { token, entries });
    });
}

fn refilter_quick_finder(s: &mut State) {
    let q = s.quick_finder.query.clone();
    if q.starts_with('>') {
        let cq = q[1..].trim().to_owned();
        if cq.is_empty() {
            s.quick_finder.filtered_commands = (0..COMMANDS.len()).collect();
        } else {
            let mut scored: Vec<(usize, i32)> = COMMANDS.iter().enumerate()
                .filter_map(|(i, cmd)| fuzzy_score(&cq, cmd.name).map(|sc| (i, sc)))
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            s.quick_finder.filtered_commands = scored.into_iter().map(|(i, _)| i).collect();
        }
        s.quick_finder.filtered = vec![];
    } else if q.is_empty() {
        s.quick_finder.filtered = (0..s.quick_finder.entries.len()).collect();
        s.quick_finder.filtered_commands = vec![];
    } else {
        let mut scored: Vec<(usize, i32)> = s.quick_finder.entries.iter().enumerate()
            .filter_map(|(i, p)| {
                let path_str = p.as_path().to_string_lossy();
                fuzzy_score_path(&q, &path_str).map(|sc| (i, sc))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        s.quick_finder.filtered = scored.into_iter().map(|(i, _)| i).collect();
        s.quick_finder.filtered_commands = vec![];
    }
    s.quick_finder.selected = 0;
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
                let name = path_str.rsplit('/').next().unwrap_or(&path_str);
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

// ── Collab message dispatcher ─────────────────────────────────────────────────

fn handle_collab_msg(s: &mut State, from_site_id: u64, msg: collab::CollabMsg) {
    use collab::{CollabMsg, CollabRole};
    match msg {
        CollabMsg::Op { site_id, clock, mut ops, path: _ } => {
            // Extract what we need before splitting borrows
            let (is_host, local_site_id) = {
                let Some(session) = s.collab.as_mut() else { return };
                let is_host = matches!(session.role, CollabRole::Host { .. });
                if !is_host {
                    // Guest: rebase remote ops against our inflight queue
                    collab::integrate_remote_against_inflight(&mut ops, &mut session.inflight);
                }
                (is_host, session.site_id)
            };

            // Apply ops to the active editor tab
            let ap = s.active_pane;
            if let Some(pane) = s.panes.get_mut(&ap) {
                if pane.kind == PaneKind::Editor {
                    if let Some(tab) = pane.tabs.get_mut(pane.active) {
                        collab::apply_ops_to_rope(&mut tab.text, &ops);
                        tab.dirty = true;
                        tab.hl_dirty_from = 0;
                        tab.hl_color_cache.clear();
                        tab.edit_generation += 1;
                        tab.max_line_len = None;
                        // Adjust local cursors so they stay in the right place
                        for cursor in &mut tab.cursors {
                            let (nh, nt) = collab::adjust_cursor(
                                cursor.head, cursor.tail, &ops, local_site_id,
                            );
                            cursor.head = nh;
                            cursor.tail = nt;
                        }
                    }
                }
            }

            // Adjust remote cursor positions for the op sender
            if let Some(session) = s.collab.as_mut() {
                if let Some(remote) = session.remote_cursors.get_mut(&site_id) {
                    for rc in remote.iter_mut() {
                        let (nh, nt) = collab::adjust_cursor(rc.head, rc.tail, &ops, 0);
                        rc.head = nh;
                        rc.tail = nt;
                    }
                }
            }

            // Host: sequence the op, broadcast to other guests, send Ack back
            if is_host {
                let Some(session) = s.collab.as_mut() else { return };
                session.server_clock += 1;
                let ack_clock = clock;  // ack the client's own clock
                let broadcast_msg = CollabMsg::Op { site_id, clock, ops, path: String::new() };
                session.broadcast_except_msg(Some(site_id), &broadcast_msg);
                session.send_to_site(site_id, &CollabMsg::Ack { clock: ack_clock });
            }
        }
        CollabMsg::Ack { clock } => {
            if let Some(session) = s.collab.as_mut() {
                session.inflight.retain(|op| op.clock > clock);
            }
        }
        CollabMsg::CursorUpdate { site_id, cursors } => {
            if let Some(session) = s.collab.as_mut() {
                session.remote_cursors.insert(site_id, cursors);
            }
        }
        CollabMsg::PeerJoined { peer } => {
            let name = peer.name.clone();
            if let Some(session) = s.collab.as_mut() {
                if !session.peers.iter().any(|p| p.site_id == peer.site_id) {
                    session.peers.push(peer);
                }
            }
            s.status_msg = Some(format!("{name} joined the session"));
        }
        CollabMsg::PeerLeft { site_id } => {
            if let Some(session) = s.collab.as_mut() {
                session.peers.retain(|p| p.site_id != site_id);
                session.remote_cursors.remove(&site_id);
            }
        }
        _ => {}
    }
    let _ = from_site_id; // used for logging in future
}

fn execute_command(s: &mut State, action: CommandAction) {
    match action {
        CommandAction::Save           => {
            let id = s.active_pane;
            if s.panes.get(&id).map_or(false, |p| p.kind == PaneKind::Editor) {
                let proxy = s.proxy.clone();
                s.tab_mut().save_with_proxy(Some(&proxy));
                let lang = s.tab().path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);
                let path = s.tab().path.clone();
                // Format-on-save only runs for local files.
                let path_str = path.as_ref()
                    .and_then(|p| p.as_local_path())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();

                let fmt_globs = s.settings.format_on_save.clone();
                if !fmt_globs.is_empty() && lang != Lang::None {
                    let matches = fmt_globs.split(',').map(|g| g.trim()).any(|g| glob_match(g, &path_str));
                    if matches {
                        match lang {
                            Lang::Json | Lang::Jsonc => format_json_document(s),
                            _ => {
                                if let (Some(srv), Some(ref p)) = (s.lsp.server_for_lang_mut(lang), path.as_ref()) {
                                    if srv.initialized { lsp::request_formatting(srv, p); }
                                }
                            }
                        }
                    }
                }

                let org_globs = s.settings.organize_imports_on_save.clone();
                if !org_globs.is_empty() && lang != Lang::None {
                    let matches = org_globs.split(',').map(|g| g.trim()).any(|g| glob_match(g, &path_str));
                    if matches {
                        if let (Some(srv), Some(ref p)) = (s.lsp.server_for_lang_mut(lang), path.as_ref()) {
                            if srv.initialized { lsp::request_organize_imports(srv, p); }
                        }
                    }
                }

                let fmt_cmd = s.settings.format_command.clone();
                if !fmt_cmd.is_empty() {
                    // External format command only supported for local files.
                    if let Some(ref p) = path {
                        if let Some(local) = p.as_local_path() {
                            let path_clone = p.clone();
                            let local_str = local.to_string_lossy().into_owned();
                            let proxy = s.proxy.clone();
                            let cmd_str = if fmt_cmd.contains("{file}") {
                                fmt_cmd.replace("{file}", &local_str)
                            } else {
                                format!("{} {}", fmt_cmd, local_str)
                            };
                            std::thread::spawn(move || {
                                let _ = std::process::Command::new("sh").arg("-c").arg(&cmd_str).status();
                                let _ = proxy.send_event(UserEvent::FormatterDone { path: path_clone });
                            });
                        }
                    }
                }
            }
        }
        CommandAction::CloseTab       => { /* handled by Cmd+W */ }
        CommandAction::GoToSettings   => open_settings_tab(s),
        CommandAction::OpenTerminal   => open_terminal_pane(s),
        CommandAction::SplitRight     => {
            let active = s.active_pane;
            let new_id = s.next_pane_id; s.next_pane_id += 1;
            let bid    = s.next_buf_id;  s.next_buf_id += 1;
            let mut p  = Pane::new(new_id, bid);
            p.tabs[0].text = s.tab().text.clone();
            p.tabs[0].path = s.tab().path.clone();
            p.tabs[0].buf_id = s.tab().buf_id;
            s.panes.insert(new_id, p);
            let old = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
            s.pane_tree = insert_pane(old, active, new_id, DropZone::Right);
            s.active_pane = new_id;
        }
        CommandAction::SplitDown      => {
            let active = s.active_pane;
            let new_id = s.next_pane_id; s.next_pane_id += 1;
            let bid    = s.next_buf_id;  s.next_buf_id += 1;
            let mut p  = Pane::new(new_id, bid);
            p.tabs[0].text = s.tab().text.clone();
            p.tabs[0].path = s.tab().path.clone();
            p.tabs[0].buf_id = s.tab().buf_id;
            s.panes.insert(new_id, p);
            let old = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
            s.pane_tree = insert_pane(old, active, new_id, DropZone::Bottom);
            s.active_pane = new_id;
        }
        CommandAction::SplitLeft      => {
            let active = s.active_pane;
            let new_id = s.next_pane_id; s.next_pane_id += 1;
            let bid    = s.next_buf_id;  s.next_buf_id += 1;
            let mut p  = Pane::new(new_id, bid);
            p.tabs[0].text = s.tab().text.clone();
            p.tabs[0].path = s.tab().path.clone();
            p.tabs[0].buf_id = s.tab().buf_id;
            s.panes.insert(new_id, p);
            let old = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
            s.pane_tree = insert_pane(old, active, new_id, DropZone::Left);
            s.active_pane = new_id;
        }
        CommandAction::SplitUp        => {
            let active = s.active_pane;
            let new_id = s.next_pane_id; s.next_pane_id += 1;
            let bid    = s.next_buf_id;  s.next_buf_id += 1;
            let mut p  = Pane::new(new_id, bid);
            p.tabs[0].text = s.tab().text.clone();
            p.tabs[0].path = s.tab().path.clone();
            p.tabs[0].buf_id = s.tab().buf_id;
            s.panes.insert(new_id, p);
            let old = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
            s.pane_tree = insert_pane(old, active, new_id, DropZone::Top);
            s.active_pane = new_id;
        }
        CommandAction::ToggleFind     => { s.find_mut().open = !s.find().open; }
        CommandAction::ToggleReplace  => {
            let open = s.find().open;
            s.find_mut().open = true;
            s.find_mut().replace_open = !open || !s.find().replace_open;
        }
        CommandAction::ToggleExplorer => {
            if s.explorer.is_none() {
                let root = std::env::current_dir().unwrap_or_default();
                s.explorer = Some(FileExplorer::new(VPath::Local(root)));
            } else {
                s.explorer = None;
            }
        }
        CommandAction::ToggleSidebar => {
            if s.explorer.is_some() {
                s.left_panel_visible = !s.left_panel_visible;
            }
        }
        CommandAction::IncreaseFontSize => {
            s.font_size = (s.font_size + 1.0).min(40.0);
            s.glyphs.resize(s.font_size);
            s.settings.font_size = s.font_size;
            s.settings.save();
        }
        CommandAction::DecreaseFontSize => {
            s.font_size = (s.font_size - 1.0).max(6.0);
            s.glyphs.resize(s.font_size);
            s.settings.font_size = s.font_size;
            s.settings.save();
        }
        CommandAction::GotoDefinition => {
            let lang  = s.tab().path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);
            let path  = s.tab().path.clone();
            let pos   = s.tab().primary().head;
            let text  = s.tab().text.clone();
            let line  = text.char_to_line(pos.min(text.len_chars().saturating_sub(1)));
            let col   = pos.saturating_sub(text.line_to_char(line));
            if lang != Lang::None {
                if let (Some(srv), Some(ref p)) = (s.lsp.server_for_lang_mut(lang), path.as_ref()) {
                    if srv.initialized { lsp::request_definition(srv, p, line, col); }
                }
            }
        }
        CommandAction::FindReferences => {
            let lang  = s.tab().path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);
            let path  = s.tab().path.clone();
            let pos   = s.tab().primary().head;
            let text  = s.tab().text.clone();
            let line  = text.char_to_line(pos.min(text.len_chars().saturating_sub(1)));
            let col   = pos.saturating_sub(text.line_to_char(line));
            if lang != Lang::None {
                if let (Some(srv), Some(ref p)) = (s.lsp.server_for_lang_mut(lang), path.as_ref()) {
                    if srv.initialized { lsp::request_references(srv, p, line, col); }
                }
            }
        }
        CommandAction::Copy => {
            let order = s.cursor_order_ltr();
            let texts: Vec<String> = order.iter()
                .filter(|&&i| s.tab().cursors[i].has_sel())
                .map(|&i| { let lo = s.tab().cursors[i].lo(); let hi = s.tab().cursors[i].hi(); s.tab().text.slice(lo..hi).chars().collect() })
                .collect();
            if !texts.is_empty() { clipboard_set(&texts.join("\n")); }
        }
        CommandAction::Cut => {
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
                        let lo = (s.tab().cursors[i].lo() as isize + delta) as usize;
                        let hi = (s.tab().cursors[i].hi() as isize + delta) as usize;
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
            if let Some(text) = clipboard_get() {
                for ch in text.chars() { s.glyphs.load(ch); }
                s.insert_str(&text);
            }
        }
        CommandAction::CursorBack    => cursor_go_back(s),
        CommandAction::CursorForward => cursor_go_fwd(s),
        CommandAction::FormatDocument => {
            let lang = s.tab().path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);
            match lang {
                Lang::Json | Lang::Jsonc => format_json_document(s),
                Lang::None => {}
                _ => {
                    let path = s.tab().path.clone();
                    if let (Some(srv), Some(ref p)) = (s.lsp.server_for_lang_mut(lang), path.as_ref()) {
                        if srv.initialized { lsp::request_formatting(srv, p); }
                    }
                }
            }
        }
        CommandAction::OrganizeImports => {
            let lang = s.tab().path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);
            let path = s.tab().path.clone();
            if lang != Lang::None {
                if let (Some(srv), Some(ref p)) = (s.lsp.server_for_lang_mut(lang), path.as_ref()) {
                    if srv.initialized { lsp::request_organize_imports(srv, p); }
                }
            }
        }
        CommandAction::OpenMarkdownPreview => open_markdown_preview(s),
        CommandAction::OpenRemoteDirectory => {
            // The command palette shows this entry; the user then types
            // an ssh:// URI in the quick-finder input.  Here we pre-fill
            // the quick-finder with an ssh:// prompt so the user can type
            // the URI directly.  If a URI is already in the quick-finder
            // input, open it immediately.
            let query = s.quick_finder.query.trim().to_owned();
            if query.starts_with("ssh://") {
                let vpath = VPath::parse(&query);
                if let VPath::Remote { ref host, .. } = vpath {
                    let h = host.clone();
                    s.explorer = Some(FileExplorer::new(vpath.clone()));
                    s.explorer_w = ((200.0 * s.font_size / FONT_PX).round() as i32).clamp(80, 600);
                    // Save to recent hosts
                    let uri = query.clone();
                    if !s.settings.recent_remote_hosts.contains(&uri) {
                        s.settings.recent_remote_hosts.insert(0, uri);
                        s.settings.recent_remote_hosts.truncate(20);
                        s.settings.save();
                    }
                    ssh::ensure_control_master(h, s.proxy.clone());
                }
                s.quick_finder.open = false;
            } else {
                // Open quick-finder pre-filled with ssh:// so user can type a URI
                s.quick_finder.query = "ssh://".to_owned();
                s.quick_finder.cursor = s.quick_finder.query.len();
                s.quick_finder.open = true;
                s.quick_finder.entries = s.settings.recent_remote_hosts.iter()
                    .map(|u| VPath::parse(u))
                    .collect();
                s.quick_finder.filtered = (0..s.quick_finder.entries.len()).collect();
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
            match collab::start_host(port, doc_path, s.proxy.clone()) {
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
                s.quick_finder.open = true;
                s.quick_finder.entries = vec![];
                s.quick_finder.filtered = vec![];
                s.quick_finder.selected = 0;
                s.quick_finder.loading = false;
            }
        }
    }
}

// ── Global find/replace ───────────────────────────────────────────────────────

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

fn refresh_git_status(proxy: winit::event_loop::EventLoopProxy<UserEvent>, root: PathBuf) {
    std::thread::spawn(move || {
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output();
        let (staged, unstaged, is_git_repo) = match out {
            Err(_) => (vec![], vec![], false),
            Ok(o) if o.status.code() == Some(128) => (vec![], vec![], false),
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout).into_owned();
                let mut staged   = vec![];
                let mut unstaged = vec![];
                for line in text.lines() {
                    if line.len() < 4 { continue; }
                    let mut chars = line.chars();
                    let x = chars.next().unwrap_or(' ');
                    let y = chars.next().unwrap_or(' ');
                    let path_part = &line[3..];
                    let path = if (x == 'R' || y == 'R') && path_part.contains(" -> ") {
                        path_part.split(" -> ").last().unwrap_or(path_part).to_owned()
                    } else {
                        path_part.to_owned()
                    };
                    if x != ' ' && x != '?' {
                        staged.push(GitEntry { xy: (x, y), path: path.clone() });
                    }
                    if (y != ' ' && y != '?') || (x == '?' && y == '?') {
                        unstaged.push(GitEntry { xy: (x, y), path });
                    }
                }
                (staged, unstaged, true)
            }
        };
        let _ = proxy.send_event(UserEvent::GitStatusResult { staged, unstaged, is_git_repo });
    });
}

fn refresh_git_diff(
    proxy:  winit::event_loop::EventLoopProxy<UserEvent>,
    root:   PathBuf,
    path:   String,
    staged: bool,
    buf_id: usize,
) {
    std::thread::spawn(move || {
        let args: &[&str] = if staged {
            &["diff", "--cached", "--", &path]
        } else {
            &["diff", "--", &path]
        };
        let stdout = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&stdout).into_owned();
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
    // Find target editor pane
    let ap = if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Editor) {
        s.active_pane
    } else {
        let area = s.pane_area();
        let Some(id) = layout_tree(&s.pane_tree, area).into_iter()
            .find(|(id, _)| s.panes.get(id).map_or(false, |p| p.kind == PaneKind::Editor))
            .map(|(id, _)| id)
        else { return; };
        s.active_pane = id;
        id
    };
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
    let buf_id = s.next_buf_id;
    s.next_buf_id += 1;
    let mut tab = Tab::untitled(buf_id);
    tab.kind = TabKind::GitDiff;
    tab.path = Some(VPath::Local(PathBuf::from(&path)));
    s.git_diff_tabs.insert(buf_id, GitDiffTabData { path: path.clone(), staged, lines: vec![], loading: true });
    let pane = s.panes.get_mut(&ap).unwrap();
    pane.tabs.push(tab);
    pane.active = pane.tabs.len() - 1;
    // Fetch diff in background (local only — remote git deferred)
    let root = s.explorer.as_ref()
        .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
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
    let mut results = Vec::new();
    for local_path in &all_files {
        let rel = local_path.strip_prefix(root).unwrap_or(local_path).to_string_lossy().to_string();
        if !include.is_empty() && !glob_match(include, &rel) { continue; }
        if !exclude.is_empty() && glob_match(exclude, &rel) { continue; }
        if std::fs::metadata(local_path).map(|m| m.len()).unwrap_or(0) > 64 * 1024 * 1024 { continue; }
        let Ok(content) = std::fs::read_to_string(local_path) else { continue };
        let path = VPath::Local(local_path.clone());
        for (line_num, line) in content.lines().enumerate() {
            let search_in = if case_sensitive { line.to_string() } else { line.to_lowercase() };
            let pattern   = if case_sensitive { query.to_string() } else { query.to_lowercase() };
            let mut start = 0;
            while let Some(pos) = search_in[start..].find(&pattern) {
                let match_col = start + pos;
                results.push(GlobalFindResult {
                    path:      path.clone(),
                    line_num,
                    line_text: line.chars().take(200).collect(),
                    match_col,
                    match_len: query.len(),
                });
                start += pos + pattern.len().max(1);
                if results.len() >= 1000 { return results; }
            }
        }
    }
    results
}

fn global_replace(results: &[GlobalFindResult], query: &str, replace: &str, case_sensitive: bool, open_tabs: &mut HashMap<usize, Pane>) {
    // Group by local path (remote replace not yet implemented).
    let mut by_path: HashMap<&std::path::Path, Vec<&GlobalFindResult>> = HashMap::new();
    for r in results {
        if let Some(local) = r.path.as_local_path() {
            by_path.entry(local).or_default().push(r);
        }
    }
    for (path, mut matches) in by_path {
        let Ok(content) = std::fs::read_to_string(path) else { continue };
        let lines: Vec<&str> = content.lines().collect();
        let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        // sort by line descending, then col descending for safe replacement
        matches.sort_by(|a, b| b.line_num.cmp(&a.line_num).then(b.match_col.cmp(&a.match_col)));
        for m in &matches {
            if m.line_num >= new_lines.len() { continue; }
            let line = &new_lines[m.line_num];
            let col = m.match_col;
            let qlen = if case_sensitive { query.len() } else { query.len() };
            if col + qlen > line.len() { continue; }
            let new_line = format!("{}{}{}", &line[..col], replace, &line[col+qlen..]);
            new_lines[m.line_num] = new_line;
        }
        let new_content = new_lines.join("\n");
        let _ = std::fs::write(path, &new_content);
        // Refresh any open tabs matching this path
        let vpath = VPath::Local(path.to_path_buf());
        for pane in open_tabs.values_mut() {
            for tab in pane.tabs.iter_mut() {
                if tab.path.as_ref() == Some(&vpath) {
                    tab.text = Rope::from_str(&new_content);
                    tab.dirty = false;
                }
            }
        }
    }
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
}

impl App {
    fn new(file_arg: Option<VPath>, dir_arg: Option<VPath>, proxy: EventLoopProxy<UserEvent>) -> Self {
        Self { state: None, file_arg, dir_arg, proxy, dirty: Arc::new(AtomicBool::new(false)), display_link: None }
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

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _el: &ActiveEventLoop, cause: StartCause) {
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

    fn resumed(&mut self, el: &ActiveEventLoop) {
        let mut attrs = WindowAttributes::default().with_title("local-text");
        #[cfg(target_os = "macos")]
        { attrs = attrs.with_title_hidden(true).with_titlebar_transparent(true); }

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

        if let Some(path) = file_arg {
            if !initial_pane.tabs[0].load_file(path.clone()) && path.as_local_path().is_some() {
                startup_status = Some(format!("File too large to open (>256 MB): {path}"));
            }
            // Remote files: async load triggered via ensure_control_master → SshConnected
            // The tab has path set but empty text; RemoteFileContent will fill it in.
        }
        let mut panes = HashMap::new();
        panes.insert(0usize, initial_pane);

        let explorer = dir_arg.map(FileExplorer::new);
        let sz = win.inner_size();

        let s = State {
            win,
            renderer,
            w: sz.width,
            h: sz.height,
            pane_tree:    PaneTree::Leaf(0),
            panes,
            active_pane:  0,
            next_pane_id: 1,
            next_buf_id:  1,
            drag:         None,
            drag_pending: None,
            resize_drag:  None,
            term_panes:  HashMap::new(),
            lsp_panes:   HashMap::new(),
            md_panes:    HashMap::new(),
            lsp:           lsp::LspManager::new(),
            diagnostics:   HashMap::new(),
            lsp_installed: HashMap::new(),
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
            diag_panel_sel: 0,
            context_menu: None,
            quick_finder: QuickFinder {
                open: false, query: String::new(), cursor: 0, sel_anchor: None,
                entries: vec![], filtered: vec![], filtered_commands: vec![], selected: 0,
                restore_tree_focus: false, walk_token: 0, loading: false,
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

            collab:        None,
            collab_before: None,
        };

        s.win.request_redraw();
        self.state = Some(s);
        self.apply_vsync_setting();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: winit::window::WindowId, event: WindowEvent) {
        let Some(s) = self.state.as_mut() else { return };
        if s.panes.is_empty() { return; }

        match event {
            WindowEvent::CloseRequested => { s.panes.clear(); el.exit(); }

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
                    let sc_w    = cm.items.iter().filter(|i| i.action != CtxAction::Separator).map(|i| i.shortcut.chars().count()).max().unwrap_or(0) as i32;
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
                    let on_border = mx >= area.x && my >= area.y && my < area.y + area.h
                        && find_border_at(&s.pane_tree, area, mx, my).is_some();
                    if on_border {
                        let axis = find_border_at(&s.pane_tree, area, mx, my).unwrap().1;
                        s.win.set_cursor(if axis == Axis::H { CursorIcon::EwResize } else { CursorIcon::NsResize });
                    } else {
                        let r     = s.active_pane_rect();
                        let fh    = s.find_h();
                        let in_ed = my >= r.y + s.tab_h() && my < r.y + r.h - fh && mx >= r.x;
                        let cmd   = s.mods.super_key();
                        // When Cmd is held and mouse is in the editor content area, use pointer cursor
                        // to signal that Cmd+Click will navigate (goto definition)
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
                if s.mouse_down && s.panes.contains_key(&s.active_pane) {
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
                                    let pty_fd = tp.pty_fd;
                                    let bytes = terminal::encode_mouse(term_col, term_row, cb, true, sgr);
                                    // SAFETY: pty_fd is valid (>= 0 checked); bytes slice lives for this synchronous call.
                                    if pty_fd >= 0 { let _ = unsafe { libc::write(pty_fd, bytes.as_ptr().cast(), bytes.len()) }; }
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
                                    let pty_fd = tp.pty_fd;
                                    let bytes = terminal::encode_mouse(term_col, term_row,
                                        if sgr { mod_bits } else { 3 | mod_bits }, false, sgr);
                                    // SAFETY: pty_fd is valid (>= 0 checked); bytes slice lives for this synchronous call.
                                    if pty_fd >= 0 { let _ = unsafe { libc::write(pty_fd, bytes.as_ptr().cast(), bytes.len()) }; }
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
                        x: mx, y: my, hovered: 0, tab_source: Some((hit.pid, hit.ti)), git_entry: None, items,
                    });
                } else {
                    // Editor text-area context menu
                    if let Some(pid) = pane_at_pos(&s.pane_tree, mx, my, area) {
                        if s.panes.get(&pid).map_or(false, |p| p.kind == PaneKind::Editor) {
                            s.active_pane = pid;
                            let lang = s.tab().path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);
                            let lsp_avail = lang != Lang::None && s.lsp.has_server_for(lang)
                                && s.lsp.server_for_lang_mut(lang).map_or(false, |srv| srv.initialized);
                            let has_sel = s.tab().cursors.iter().any(|c| c.has_sel());
                            s.context_menu = Some(ContextMenu {
                                x: mx, y: my,
                                hovered: 0,
                                tab_source: None, git_entry: None,
                                items: vec![
                                    ContextMenuItem { label: "Go to Definition",   shortcut: "Cmd+Click / F12",   action: CtxAction::GotoDefinition,  enabled: lsp_avail },
                                    ContextMenuItem { label: "Find All References", shortcut: "Cmd+Shift+F12",    action: CtxAction::FindReferences,   enabled: lsp_avail },
                                    ContextMenuItem { label: "Format Document",    shortcut: "Opt+Shift+F",       action: CtxAction::FormatDocument,  enabled: lsp_avail },
                                    ContextMenuItem { label: "Organize Imports",   shortcut: "Opt+Shift+O",       action: CtxAction::OrganizeImports, enabled: lsp_avail },
                                    ContextMenuItem { label: "",                   shortcut: "",                  action: CtxAction::Separator,       enabled: true },
                                    ContextMenuItem { label: "Copy",               shortcut: "Cmd+C",             action: CtxAction::Copy,            enabled: has_sel },
                                    ContextMenuItem { label: "Cut",                shortcut: "Cmd+X",             action: CtxAction::Cut,             enabled: has_sel },
                                    ContextMenuItem { label: "Paste",              shortcut: "Cmd+V",             action: CtxAction::Paste,           enabled: true },
                                ],
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
                let cmd = s.mods.super_key();
                let shift = s.mods.shift_key();

                // Quick finder overlay click (must run before other handlers)
                if s.quick_finder.open {
                    let cw = s.glyphs.cw;
                    let lh = s.glyphs.lh;
                    let ow = (s.w as i32 * 2 / 3).min(s.w as i32 - 40).max(360);
                    let ox = (s.w as i32 - ow) / 2;
                    let oy = s.h as i32 / 4;
                    let item_count = if s.quick_finder.query.starts_with('>') {
                        s.quick_finder.filtered_commands.len()
                    } else { s.quick_finder.filtered.len() };
                    let oh = lh * (item_count as i32 + 2) + 8;
                    if mx >= ox && mx < ox + ow && my >= oy && my < oy + oh {
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
                    let sc_w    = cm.items.iter().filter(|i| i.action != CtxAction::Separator).map(|i| i.shortcut.chars().count()).max().unwrap_or(0) as i32;
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
                        s.context_menu = None;
                        if let Some(action) = action_taken {
                            let root_ga = s.explorer.as_ref()
                                .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                            match action {
                                CtxAction::GitOpenFile => {
                                    if let Some((_, ref path)) = git_entry {
                                        let pb = VPath::Local(root_ga.join(path));
                                        open_or_reuse_tab(s, pb);
                                    }
                                }
                                CtxAction::GitViewDiff => {
                                    if let Some((staged, ref path)) = git_entry {
                                        open_diff_tab(s, path.clone(), staged);
                                    }
                                }
                                CtxAction::GitStage => {
                                    if let Some((_, ref path)) = git_entry {
                                        let path = path.clone(); let proxy = s.proxy.clone(); let root = root_ga;
                                        std::thread::spawn(move || {
                                            let _ = std::process::Command::new("git")
                                                .args(["add", "--", &path]).current_dir(&root).status();
                                            let _ = proxy.send_event(UserEvent::GitOpDone);
                                        });
                                    }
                                }
                                CtxAction::GitUnstage => {
                                    if let Some((_, ref path)) = git_entry {
                                        let path = path.clone(); let proxy = s.proxy.clone(); let root = root_ga;
                                        std::thread::spawn(move || {
                                            let _ = std::process::Command::new("git")
                                                .args(["restore", "--staged", "--", &path]).current_dir(&root).status();
                                            let _ = proxy.send_event(UserEvent::GitOpDone);
                                        });
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
                                        if s.panes.get(&pid).map_or(0, |p| p.tabs.len()) > 1 {
                                            let pane = s.panes.get_mut(&pid).unwrap();
                                            let closed = pane.tabs.remove(ti);
                                            if let Some(ref p) = closed.path {
                                                let lang = Lang::from_path(p.as_path());
                                                if let Some(srv) = s.lsp.server_for_lang_mut(lang) {
                                                    lsp::notify_did_close(srv, p);
                                                }
                                            }
                                            if pane.active >= pane.tabs.len() { pane.active = pane.tabs.len() - 1; }
                                        } else if s.panes.len() > 1 {
                                            s.md_panes.remove(&pid);
                                            s.panes.remove(&pid);
                                            let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                                            if let Some(t) = remove_pane_from_tree(old_tree, pid) { s.pane_tree = t; }
                                            s.active_pane = layout_tree(&s.pane_tree, s.pane_area()).first().map(|(id, _)| *id).unwrap_or(0);
                                        } else {
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
                    let file_icon_y = 8;
                    let srch_icon_y = file_icon_y + lh + 4;
                    let diag_icon_y = srch_icon_y + lh + 4;
                    let git_icon_y  = diag_icon_y + lh + 4;
                    let gear_y      = s.h as i32 - s.status_h() - lh - 8;
                    let clicked_view = if my >= file_icon_y && my < file_icon_y + lh {
                        Some(LeftView::FileTree)
                    } else if my >= srch_icon_y && my < srch_icon_y + lh {
                        Some(LeftView::GlobalSearch)
                    } else if my >= diag_icon_y && my < diag_icon_y + lh {
                        Some(LeftView::Diagnostics)
                    } else if my >= git_icon_y && my < git_icon_y + lh {
                        Some(LeftView::Git)
                    } else {
                        None
                    };
                    if let Some(view) = clicked_view {
                        if s.left_panel_visible && s.left_view == view {
                            s.left_panel_visible = false;
                        } else {
                            s.left_view = view;
                            s.left_panel_visible = true;
                            if view == LeftView::GlobalSearch {
                                s.global_find.focus = GlobalFindFocus::Query;
                            }
                            if view == LeftView::Git {
                                let root = s.explorer.as_ref()
                                    .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                                s.git_panel.loading = true;
                                refresh_git_status(s.proxy.clone(), root);
                            } else {
                                s.git_panel.commit_focused = false;
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
                                tab_source: None, git_entry: None,
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
                                    if let Some(local) = ex.root.as_local_path() {
                                        let local = local.to_path_buf();
                                        walk_files(&local, &mut ex.tree_search_entries, 0);
                                    }
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
                            let idx = (row - 2) as usize;
                            let in_search = s.explorer.as_ref().map_or(false, |ex| !ex.tree_search.is_empty());
                            if in_search {
                                let path = s.explorer.as_ref()
                                    .and_then(|ex| ex.tree_search_results.get(idx).cloned());
                                if let Some(path) = path { open_or_reuse_tab(s, VPath::Local(path)); }
                            } else {
                                // Unfocus tree search if clicking entries
                                if let Some(ex) = s.explorer.as_mut() { ex.tree_search_focused = false; }
                                let action = s.explorer.as_mut().and_then(|ex| {
                                    if idx < ex.entries.len() {
                                        ex.selected = idx;
                                        if ex.entries[idx].is_dir { ex.toggle(idx); None }
                                        else { Some(ex.entries[idx].path.clone()) }
                                    } else { None }
                                });
                                if let Some(path) = action { open_or_reuse_tab(s, VPath::Local(path)); }
                            }
                        }
                    } else if s.left_view == LeftView::Diagnostics {
                        // Diagnostics panel click — open file and jump to line
                        let header_rows = 2; // header label + separator
                        let first_item_y = header_rows * lh + 2 + 4;
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
                                let (_, path, line) = sev_items[idx].clone();
                                open_or_reuse_tab(s, path);
                                let pos = s.tab().text.line_to_char(line);
                                s.tab_mut().cursors = vec![Cursor::new(pos)];
                                s.ensure_visible();
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
                                s.git_panel.sel = GitSel::Staged(i);
                                let path = s.git_panel.staged[i].path.clone();
                                let root = s.explorer.as_ref()
                                    .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                                let proxy = s.proxy.clone();
                                std::thread::spawn(move || {
                                    let _ = std::process::Command::new("git")
                                        .args(["restore", "--staged", "--", &path])
                                        .current_dir(&root)
                                        .status();
                                    let _ = proxy.send_event(UserEvent::GitOpDone);
                                });
                            }
                        } else if my >= unstaged_start_y && my < commit_area_top {
                            let unstaged_count = s.git_panel.unstaged.len();
                            let i = ((my - unstaged_start_y) / lh) as usize;
                            if i < unstaged_count {
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
                                let root = s.explorer.as_ref()
                                    .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                                let proxy = s.proxy.clone();
                                s.git_panel.commit_msg.clear();
                                s.git_panel.commit_cursor = 0;
                                s.git_panel.commit_focused = false;
                                std::thread::spawn(move || {
                                    let _ = std::process::Command::new("git")
                                        .args(["commit", "-m", &msg])
                                        .current_dir(&root)
                                        .status();
                                    let _ = proxy.send_event(UserEvent::GitOpDone);
                                });
                            } else if mx >= act_w + explorer_w / 2 && has_unstaged {
                                let root = s.explorer.as_ref()
                                    .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                                let proxy = s.proxy.clone();
                                std::thread::spawn(move || {
                                    let _ = std::process::Command::new("git")
                                        .args(["add", "."])
                                        .current_dir(&root)
                                        .status();
                                    let _ = proxy.send_event(UserEvent::GitOpDone);
                                });
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
                            // Search button
                            let root = s.explorer.as_ref()
                                .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                            if !s.global_find.query.is_empty() {
                                spawn_search(&mut s.global_find, root, s.proxy.clone());
                                s.global_find.focus = GlobalFindFocus::Results;
                            }
                            // Also check Replace All button
                            let search_label_w = "[Search]".chars().count() as i32 * s.glyphs.cw;
                            let ra_x = act_w + field_label_w + search_label_w + s.glyphs.cw;
                            if mx >= ra_x && !s.global_find.replace.is_empty() && !s.global_find.results.is_empty() {
                                let results = s.global_find.results.clone();
                                let query   = s.global_find.query.clone();
                                let replace = s.global_find.replace.clone();
                                let cs      = s.global_find.case_sensitive;
                                global_replace(&results, &query, &replace, cs, &mut s.panes);
                                s.global_find.results.clear();
                            }
                            } // end else (not live toggle)
                        } else if my >= results_start_y {
                            let ri = ((my - results_start_y) / lh) as usize + s.global_find.scroll;
                            if ri < s.global_find.results.len() {
                                s.global_find.selected = ri;
                                s.global_find.focus    = GlobalFindFocus::Results;
                                let r = s.global_find.results[ri].clone();
                                open_or_reuse_tab(s, r.path.clone());
                                let line = r.line_num;
                                let col  = r.match_col;
                                let pos  = s.tab().text.line_to_char(line) + col;
                                s.tab_mut().cursors = vec![Cursor::new(pos)];
                                s.ensure_visible();
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
                // Clicking any content pane removes left-panel focus
                if let Some(ex) = s.explorer.as_mut() { ex.tree_search_focused = false; }
                s.global_find.focus = GlobalFindFocus::Results;
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
                                    // × close
                                    s.active_pane = clicked_pane_id;
                                    let tid = s.panes.get_mut(&clicked_pane_id).unwrap().term_ids.remove(i);
                                    s.term_panes.remove(&tid);
                                    let pane = s.panes.get_mut(&clicked_pane_id).unwrap();
                                    if pane.term_ids.is_empty() {
                                        s.panes.remove(&clicked_pane_id);
                                        if s.panes.is_empty() {
                                            el.exit();
                                            { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                                            return;
                                        }
                                        let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                                        if let Some(t) = remove_pane_from_tree(old_tree, clicked_pane_id) {
                                            s.pane_tree = t;
                                        }
                                        let new_active = layout_tree(&s.pane_tree, s.pane_area())
                                            .first().map(|(id, _)| *id).unwrap_or(0);
                                        s.active_pane = new_active;
                                    } else {
                                        if pane.active >= pane.term_ids.len() {
                                            pane.active = pane.term_ids.len() - 1;
                                        }
                                    }
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
                                if s.panes[&pane_id].tabs.len() > 1 {
                                    let pane = s.panes.get_mut(&pane_id).unwrap();
                                    pane.tabs.remove(i);
                                    if pane.active >= pane.tabs.len() { pane.active = pane.tabs.len() - 1; }
                                } else if s.panes.len() > 1 {
                                    s.panes.remove(&pane_id);
                                    let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                                    if let Some(new_tree) = remove_pane_from_tree(old_tree, pane_id) {
                                        s.pane_tree = new_tree;
                                    }
                                    let new_active = layout_tree(&s.pane_tree, s.pane_area()).first().map(|(id, _)| *id).unwrap_or(0);
                                    s.active_pane = new_active;
                                } else {
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

                // Non-editor panes (Terminal, LspOutput) have no tabs/find bar/cursors
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
                                    let pty_fd = tp.pty_fd;
                                    let bytes = terminal::encode_mouse(term_col, term_row, mod_bits, true, sgr);
                                    // SAFETY: pty_fd is valid (>= 0 checked); bytes slice lives for this synchronous call.
                                    if pty_fd >= 0 { let _ = unsafe { libc::write(pty_fd, bytes.as_ptr().cast(), bytes.len()) }; }
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
                                            drop(rows);
                                            open_token(s, &token);
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
                            let lsp_rows: [(&str, &str); 3] = [
                                ("TypeScript", "npm install -g typescript-language-server typescript\n"),
                                ("Rust      ", "rustup component add rust-analyzer\n"),
                                ("Python    ", "pip install python-lsp-server\n"),
                            ];
                            let inst_btn_x = btn_x + 11 * cw;
                            let inst_btn_w = 9 * cw;
                            for (i, (_, cmd)) in lsp_rows.iter().enumerate() {
                                let ly = lsp_sec_y + lh + 4 + i as i32 * (lh + 4);
                                if my >= ly && my < ly + lh && mx >= inst_btn_x && mx < inst_btn_x + inst_btn_w {
                                    open_terminal_pane(s);
                                    let pane_id = s.active_pane;
                                    if let Some(&tid) = s.panes.get(&pane_id).and_then(|p| p.term_ids.get(p.active)) {
                                        if let Some(tp) = s.term_panes.get(&tid) {
                                            let bytes = cmd.as_bytes();
                                            // SAFETY: pty_fd is valid (>= 0 checked); bytes slice lives for this synchronous call.
                                            if tp.pty_fd >= 0 { unsafe { libc::write(tp.pty_fd, bytes.as_ptr().cast(), bytes.len()); } }
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
                                    };
                                    let cursor = current.chars().count();
                                    s.settings_edit_field  = Some(*fid);
                                    s.settings_edit_text   = current;
                                    s.settings_edit_cursor = cursor;
                                    clicked_field = true;
                                    break;
                                }
                            }
                            if !clicked_field {
                                // Click outside fields — commit and close any open field
                                if let Some(fid) = s.settings_edit_field {
                                    let text = s.settings_edit_text.clone();
                                    match fid {
                                        SettingsFieldId::FormatOnSave          => s.settings.format_on_save = text,
                                        SettingsFieldId::OrganizeImportsOnSave => s.settings.organize_imports_on_save = text,
                                        SettingsFieldId::FormatCommand         => s.settings.format_command = text,
                                    }
                                    s.settings.save();
                                    s.settings_edit_field = None;
                                }
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
                    let find_y  = pane_rect.y + pane_rect.h - fh;
                    let row_h   = s.glyphs.lh + 4;
                    let cw      = s.glyphs.cw;
                    let rel_row = (my - find_y) / row_h;
                    if rel_row == 0 {
                        let aa_len   = 4usize;
                        let w_len    = 3usize;
                        let toggle_w = (aa_len + 1 + w_len) as i32 * cw + 8;
                        let aa_x     = s.w as i32 - toggle_w;
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
                            f.focus = FindFocus::Query;
                            let cur_chars = f.query[..f.cursor_query].chars().count();
                            let new_byte = field_click_to_byte(&f.query, mx, qx, cw, cur_chars, q_vis);
                            f.cursor_query = new_byte;
                            f.sel_anchor_q = None;
                        }
                    } else if rel_row == 1 && s.find().replace_open {
                        let repl_len = 6usize;
                        let all_len  = 5usize;
                        let btn_w    = (repl_len + 1 + all_len) as i32 * cw + 8;
                        let btn_x    = s.w as i32 - btn_w;
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
                            f.focus = FindFocus::Replace;
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
                if cmd {
                    // Cmd+Click: try LSP goto-definition first, fallback to path token open
                    let click_line = s.tab().text.char_to_line(pos.min(s.tab().text.len_chars().saturating_sub(1)));
                    let line_start = s.tab().text.line_to_char(click_line);
                    let click_col  = pos.saturating_sub(line_start);
                    let tab_lang   = s.tab().path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);
                    let tab_path   = s.tab().path.clone();
                    let mut lsp_sent = false;
                    if tab_lang != Lang::None {
                        if let (Some(srv), Some(ref path)) = (s.lsp.server_for_lang_mut(tab_lang), tab_path.as_ref()) {
                            if srv.initialized {
                                lsp::request_definition(srv, path, click_line, click_col);
                                lsp_sent = true;
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
                            open_token(s, &token);
                        }
                    }
                } else if alt {
                    s.tab_mut().cursors.push(Cursor::new(pos));
                    s.dedup_cursors();
                    s.mouse_down = false;
                } else {
                    let now  = Instant::now();
                    let fast = now.duration_since(s.last_click_time) < Duration::from_millis(500);
                    let same = pos == s.last_click_char;
                    s.click_count = if fast && same { s.click_count + 1 } else { 1 };
                    s.last_click_time = now;
                    s.last_click_char = pos;
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
                    if dy < 0 {
                        s.global_find.scroll = (s.global_find.scroll + (-dy) as usize).min(n.saturating_sub(1));
                    } else {
                        s.global_find.scroll = s.global_find.scroll.saturating_sub(dy as usize);
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
                                let pty_fd = tp.pty_fd;
                                let term_col = ((mx - pane_rect.x) / cw)
                                    .clamp(0, tp.grid.cols as i32 - 1) as usize;
                                let term_row = ((my - content_y) / lh)
                                    .clamp(0, tp.grid.rows as i32 - 1) as usize;
                                for _ in 0..n {
                                    let bytes = terminal::encode_mouse(term_col, term_row, cb, true, sgr);
                                    // SAFETY: pty_fd is valid (>= 0 checked); bytes slice lives for this synchronous call.
                                    if pty_fd >= 0 { let _ = unsafe { libc::write(pty_fd, bytes.as_ptr().cast(), bytes.len()) }; }
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
                    PaneKind::LspOutput => {
                        if dy != 0 {
                            let op = s.lsp_panes.get_mut(&s.active_pane).unwrap();
                            let max_scroll = op.lines.len().saturating_sub(1);
                            if dy < 0 {
                                op.scroll = (op.scroll + (-dy) as usize).min(max_scroll);
                            } else {
                                op.scroll = op.scroll.saturating_sub(dy as usize);
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
                            if dy < 0 {
                                mp.scroll = (mp.scroll + (-dy) as usize).min(max_scroll);
                            } else {
                                mp.scroll = mp.scroll.saturating_sub(dy as usize);
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
                                let content_h = if s.settings.undo_limit.is_some() { 24 * lh + 104 } else { 23 * lh + 100 };
                                let max_scroll = content_h.saturating_sub(visible_h) / lh + 1;
                                let t = s.tab_mut();
                                if dy < 0 {
                                    t.scroll = (t.scroll + (-dy) as usize).min(max_scroll);
                                } else {
                                    t.scroll = t.scroll.saturating_sub(dy as usize);
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

            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                dlog!("[input] {:?}", event.logical_key);
                s.status_msg = None;
                let ctrl  = s.mods.control_key();
                let cmd   = s.mods.super_key();
                let alt   = s.mods.alt_key();
                let shift = s.mods.shift_key();

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
                if cmd && !shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "p") {
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

                // Cmd+B — toggle left panel visibility
                if cmd && !shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "b" | "B")) {
                    if s.explorer.is_some() {
                        s.left_panel_visible = !s.left_panel_visible;
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
                        s.explorer = Some(FileExplorer::new(VPath::Local(root)));
                        s.explorer_w = ((200.0 * s.font_size / FONT_PX).round() as i32).clamp(80, 600);
                    }
                    s.left_view = LeftView::GlobalSearch;
                    s.global_find.focus = GlobalFindFocus::Query;
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Tree search input routing
                if s.explorer.as_ref().map_or(false, |ex| ex.tree_search_focused)
                    && s.left_view == LeftView::FileTree
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

                // Quick finder / command palette input routing (unified)
                if s.quick_finder.open {
                    let is_cmd_mode = s.quick_finder.query.starts_with('>');
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            s.quick_finder.open = false;
                            if s.quick_finder.restore_tree_focus {
                                s.quick_finder.restore_tree_focus = false;
                                if let Some(ex) = s.explorer.as_mut() { ex.tree_search_focused = true; }
                            }
                        }
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
                            let idx = s.quick_finder.selected;
                            if is_cmd_mode {
                                if let Some(&fi) = s.quick_finder.filtered_commands.get(idx) {
                                    let action = COMMANDS[fi].action;
                                    s.quick_finder.open = false;
                                    if s.quick_finder.restore_tree_focus {
                                        s.quick_finder.restore_tree_focus = false;
                                        if let Some(ex) = s.explorer.as_mut() { ex.tree_search_focused = true; }
                                    }
                                    execute_command(s, action);
                                }
                            } else if let Some(&fi) = s.quick_finder.filtered.get(idx) {
                                let path = s.quick_finder.entries[fi].clone();
                                s.quick_finder.open = false;
                                if s.quick_finder.restore_tree_focus {
                                    s.quick_finder.restore_tree_focus = false;
                                    if let Some(ex) = s.explorer.as_mut() { ex.tree_search_focused = true; }
                                }
                                // If the selected entry is a remote path shown from "Open Remote Directory",
                                // open it as a workspace root rather than a file tab.
                                let query = s.quick_finder.query.trim().to_owned();
                                let is_remote_dir_mode = query.starts_with("ssh://")
                                    && matches!(path, VPath::Remote { .. });
                                if is_remote_dir_mode {
                                    if let VPath::Remote { ref host, .. } = path {
                                        let h = host.clone();
                                        s.explorer = Some(FileExplorer::new(path.clone()));
                                        s.explorer_w = ((200.0 * s.font_size / FONT_PX).round() as i32).clamp(80, 600);
                                        let uri = path.display_short();
                                        if !s.settings.recent_remote_hosts.contains(&uri) {
                                            s.settings.recent_remote_hosts.insert(0, uri);
                                            s.settings.recent_remote_hosts.truncate(20);
                                            s.settings.save();
                                        }
                                        ssh::ensure_control_master(h, s.proxy.clone());
                                    }
                                } else {
                                    push_cursor_history(s);
                                    open_or_reuse_tab(s, path);
                                }
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

                // Git panel commit field input routing
                if s.explorer.is_some() && s.left_view == LeftView::Git && s.git_panel.commit_focused {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            s.git_panel.commit_focused = false;
                        }
                        Key::Named(NamedKey::Enter) => {
                            let can_commit = !s.git_panel.staged.is_empty()
                                && !s.git_panel.commit_msg.is_empty();
                            if can_commit {
                                let msg = s.git_panel.commit_msg.clone();
                                let root = s.explorer.as_ref()
                                    .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                                let proxy = s.proxy.clone();
                                s.git_panel.commit_msg.clear();
                                s.git_panel.commit_cursor = 0;
                                s.git_panel.commit_focused = false;
                                std::thread::spawn(move || {
                                    let _ = std::process::Command::new("git")
                                        .args(["commit", "-m", &msg])
                                        .current_dir(&root)
                                        .status();
                                    let _ = proxy.send_event(UserEvent::GitOpDone);
                                });
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
                if s.explorer.is_some() && s.left_view == LeftView::GlobalSearch
                    && !matches!(s.global_find.focus, GlobalFindFocus::Results)
                {
                    let focus = s.global_find.focus;
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => { s.global_find.focus = GlobalFindFocus::Results; }
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
                        Key::Named(NamedKey::Enter) if focus == GlobalFindFocus::Query => {
                            let root = s.explorer.as_ref()
                                .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                            if !s.global_find.query.is_empty() {
                                spawn_search(&mut s.global_find, root, s.proxy.clone());
                                s.global_find.focus = GlobalFindFocus::Results;
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

                // Diagnostics panel keyboard navigation
                if s.explorer.is_some() && s.left_view == LeftView::Diagnostics {
                    let total: usize = s.diagnostics.values().map(|v| v.len()).sum();
                    match &event.logical_key {
                        Key::Named(NamedKey::ArrowUp) => {
                            s.diag_panel_sel = s.diag_panel_sel.saturating_sub(1);
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if total > 0 { s.diag_panel_sel = (s.diag_panel_sel + 1).min(total.saturating_sub(1)); }
                        }
                        Key::Named(NamedKey::Enter) if total > 0 => {
                            let mut sev_items: Vec<(usize, VPath, usize)> = s.diagnostics.iter()
                                .flat_map(|(path, diags)| {
                                    let sev_ord = |s: &DiagSeverity| match s { DiagSeverity::Error => 0, DiagSeverity::Warning => 1, _ => 2 };
                                    diags.iter().map(move |d| (sev_ord(&d.severity), path.clone(), d.line))
                                })
                                .collect();
                            sev_items.sort();
                            let idx = s.diag_panel_sel.min(sev_items.len().saturating_sub(1));
                            if idx < sev_items.len() {
                                let (_, path, line) = sev_items[idx].clone();
                                push_cursor_history(s);
                                open_or_reuse_tab(s, path);
                                let pos = s.tab().text.line_to_char(line);
                                s.tab_mut().cursors = vec![Cursor::new(pos)];
                                s.ensure_visible();
                            }
                        }
                        _ => {}
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Global find results navigation
                if s.explorer.is_some() && s.left_view == LeftView::GlobalSearch
                    && s.global_find.focus == GlobalFindFocus::Results
                    && !s.global_find.results.is_empty()
                {
                    match &event.logical_key {
                        Key::Named(NamedKey::ArrowUp) => {
                            s.global_find.selected = s.global_find.selected.saturating_sub(1);
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            let n = s.global_find.results.len();
                            s.global_find.selected = (s.global_find.selected + 1).min(n.saturating_sub(1));
                        }
                        Key::Named(NamedKey::Enter) => {
                            let r = s.global_find.results[s.global_find.selected].clone();
                            push_cursor_history(s);
                            open_or_reuse_tab(s, r.path.clone());
                            let line = r.line_num;
                            let col  = r.match_col;
                            let pos  = s.tab().text.line_to_char(line) + col;
                            s.tab_mut().cursors = vec![Cursor::new(pos)];
                            s.ensure_visible();
                        }
                        _ => {}
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Settings text field editing
                if let Some(field_id) = s.settings_edit_field {
                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Escape) => {
                            if matches!(&event.logical_key, Key::Named(NamedKey::Enter)) {
                                let text = s.settings_edit_text.clone();
                                match field_id {
                                    SettingsFieldId::FormatOnSave          => s.settings.format_on_save = text,
                                    SettingsFieldId::OrganizeImportsOnSave => s.settings.organize_imports_on_save = text,
                                    SettingsFieldId::FormatCommand         => s.settings.format_command = text,
                                }
                                s.settings.save();
                            }
                            s.settings_edit_field = None;
                        }
                        Key::Named(NamedKey::Backspace) => {
                            if s.settings_edit_cursor > 0 {
                                let byte_off = s.settings_edit_text.char_indices()
                                    .nth(s.settings_edit_cursor - 1).map(|(i, _)| i).unwrap_or(0);
                                s.settings_edit_text.remove(byte_off);
                                s.settings_edit_cursor -= 1;
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
                        _ => {
                            if let Some(txt) = event.text.as_deref() {
                                if !txt.chars().any(|c| c.is_control()) {
                                    let byte_off = s.settings_edit_text.char_indices()
                                        .nth(s.settings_edit_cursor).map(|(i, _)| i)
                                        .unwrap_or(s.settings_edit_text.len());
                                    s.settings_edit_text.insert_str(byte_off, txt);
                                    s.settings_edit_cursor += txt.chars().count();
                                }
                            }
                        }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Terminal pane intercepts (before PTY forwarding)
                if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Terminal) {
                    // Cmd+W — close active terminal tab or pane
                    if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "w") {
                        let pane_id = s.active_pane;
                        let n = s.panes[&pane_id].term_ids.len();
                        if n > 1 {
                            let idx = s.panes[&pane_id].active;
                            let tid = s.panes.get_mut(&pane_id).unwrap().term_ids.remove(idx);
                            s.term_panes.remove(&tid);
                            let pane = s.panes.get_mut(&pane_id).unwrap();
                            if pane.active >= pane.term_ids.len() {
                                pane.active = pane.term_ids.len().saturating_sub(1);
                            }
                        } else {
                            let tids: Vec<usize> = s.panes.get(&pane_id)
                                .map(|p| p.term_ids.clone()).unwrap_or_default();
                            for tid in tids { s.term_panes.remove(&tid); }
                            s.panes.remove(&pane_id);
                            if s.panes.is_empty() {
                                s.pane_tree  = PaneTree::Leaf(0);
                                s.active_pane = 0;
                                el.exit();
                                return;
                            }
                            let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                            if let Some(t) = remove_pane_from_tree(old_tree, pane_id) { s.pane_tree = t; }
                            let new_active = layout_tree(&s.pane_tree, s.pane_area())
                                .first().map(|(id, _)| *id).unwrap_or(0);
                            s.active_pane = new_active;
                        }
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                        return;
                    }
                    // Ctrl+Shift+5 — split: new terminal with same shell below
                    if ctrl && shift && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "5" | "%")) {
                        // If in a remote workspace, prefer SSH over duplicating the local shell.
                        let shell = if let Some(host) = active_workspace_ssh_host(s) {
                            format!(
                                "ssh -o ControlPath={} {}",
                                host.control_path().display(),
                                host.host_arg(),
                            )
                        } else {
                            let p = &s.panes[&s.active_pane];
                            p.term_ids.get(p.active)
                                .and_then(|&tid| s.term_panes.get(&tid).map(|tp| tp.shell.clone()))
                                .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()))
                        };
                        let pane_id = s.next_pane_id; s.next_pane_id += 1;
                        let term_id = s.next_pane_id; s.next_pane_id += 1;
                        let area = s.pane_area();
                        let cols = (area.w / s.glyphs.cw).max(1) as usize;
                        let rows = ((area.h / 2) / s.glyphs.lh).max(1) as usize;
                        let proxy = s.proxy.clone();
                        let tp = terminal::spawn_terminal_with_shell(term_id, cols, rows, proxy, Some(shell));
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
                    // Cmd+C — copy terminal selection (or fall through when term_copy_paste disabled)
                    if cmd && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "c" | "C")) {
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
                        // No selection: if copy/paste enabled, swallow here; otherwise fall through to PTY
                        if s.settings.term_copy_paste { return; }
                    }
                    // Cmd+V — paste from clipboard (only when term_copy_paste enabled)
                    if s.settings.term_copy_paste && cmd && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "v" | "V")) {
                        s.term_sel = None;
                        let text = clipboard_read();
                        if !text.is_empty() {
                            let p = &s.panes[&s.active_pane];
                            if let Some(&tid) = p.term_ids.get(p.active) {
                                if let Some(tp) = s.term_panes.get(&tid) {
                                    let bytes = text.as_bytes();
                                    // SAFETY: pty_fd is valid (>= 0 checked); bytes slice lives for this synchronous call.
                                    if tp.pty_fd >= 0 { let _ = unsafe { libc::write(tp.pty_fd, bytes.as_ptr().cast(), bytes.len()) }; }
                                }
                            }
                        }
                        return;
                    }
                    // Cmd+Backspace → send \x15 (kill to line start) when enabled
                    if s.settings.term_cmd_bs && cmd && matches!(&event.logical_key, Key::Named(NamedKey::Backspace)) {
                        let p = &s.panes[&s.active_pane];
                        if let Some(&tid) = p.term_ids.get(p.active) {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                // SAFETY: pty_fd is valid (>= 0 checked); byte literal lives for this synchronous call.
                                if tp.pty_fd >= 0 { let _ = unsafe { libc::write(tp.pty_fd, b"\x15".as_ptr().cast(), 1) }; }
                            }
                        }
                        return;
                    }
                    // Alt/Option+Backspace → send \x17 (delete previous word) when enabled
                    if s.settings.term_alt_bs && alt && matches!(&event.logical_key, Key::Named(NamedKey::Backspace)) {
                        let p = &s.panes[&s.active_pane];
                        if let Some(&tid) = p.term_ids.get(p.active) {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                // SAFETY: pty_fd is valid (>= 0 checked); byte literal lives for this synchronous call.
                                if tp.pty_fd >= 0 { let _ = unsafe { libc::write(tp.pty_fd, b"\x17".as_ptr().cast(), 1) }; }
                            }
                        }
                        return;
                    }
                    // Forward all other key events to the active PTY
                    s.term_sel = None;
                    let bytes = terminal::encode_key(&event.logical_key, s.mods, event.text.as_deref());
                    if let Some(bytes) = bytes {
                        let p = &s.panes[&s.active_pane];
                        if let Some(&tid) = p.term_ids.get(p.active) {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                // SAFETY: pty_fd is valid (>= 0 checked); bytes slice lives for this synchronous call.
                                if tp.pty_fd >= 0 { let _ = unsafe { libc::write(tp.pty_fd, bytes.as_ptr().cast(), bytes.len()) }; }
                            }
                        }
                    }
                    return;
                }

                // MarkdownPreview pane: Cmd+W closes it, all other input ignored
                if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::MarkdownPreview) {
                    if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "w") {
                        let pane_id = s.active_pane;
                        s.md_panes.remove(&pane_id);
                        s.panes.remove(&pane_id);
                        if s.panes.is_empty() {
                            s.pane_tree  = PaneTree::Leaf(0);
                            s.active_pane = 0;
                            el.exit();
                            return;
                        }
                        let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                        if let Some(t) = remove_pane_from_tree(old_tree, pane_id) { s.pane_tree = t; }
                        let new_active = layout_tree(&s.pane_tree, s.pane_area())
                            .first().map(|(id, _)| *id).unwrap_or(0);
                        s.active_pane = new_active;
                        { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    }
                    return;
                }

                // LspOutput pane: read-only, ignore all input except pane-switch shortcuts
                if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::LspOutput) {
                    return;
                }

                // Find bar: intercept editing keys (including cmd variants) when open
                let find_handled = if s.find().open {
                    let key = &event.logical_key;
                    match key {
                        Key::Named(NamedKey::Escape) => { s.find_mut().open = false; true }
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
                    // Cmd+W for settings tab: close the tab (never dirty, no LSP needed)
                    if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "w") {
                        let pane_id = s.active_pane;
                        if s.panes.get(&pane_id).and_then(|p| p.tabs.get(p.active))
                                .map_or(false, |t| t.kind == TabKind::Settings) {
                            let pane = s.panes.get_mut(&pane_id).unwrap();
                            pane.tabs.remove(pane.active);
                            if pane.active >= pane.tabs.len() && !pane.tabs.is_empty() {
                                pane.active = pane.tabs.len() - 1;
                            }
                            if !pane.tabs.is_empty() {
                                s.needs_redraw = true;
                                self.dirty.store(true, Ordering::Release);
                                return;
                            }
                            // else: no tabs left → fall through to normal Cmd+W pane-close logic
                        }
                    }

                    // Block text editing in settings tabs
                    let active_tab_is_settings = s.panes.get(&s.active_pane)
                        .and_then(|p| p.tabs.get(p.active))
                        .map_or(false, |t| t.kind == TabKind::Settings);

                    // Main editor keyboard handling
                    let handled =
                        if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "a") {
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
                                            let lo = (s.tab().cursors[i].lo() as isize + delta) as usize;
                                            let hi = (s.tab().cursors[i].hi() as isize + delta) as usize;
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
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "v") {
                            if let Some(text) = clipboard_get() {
                                for ch in text.chars() { s.glyphs.load(ch); }
                                let n_cursors = s.tab().cursors.len();
                                let lines: Vec<&str> = text.split('\n').collect();
                                if n_cursors > 1 && lines.len() == n_cursors {
                                    let owned: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
                                    s.insert_str_per_cursor(&owned);
                                } else {
                                    s.insert_str(&text);
                                }
                            }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "o") {
                            if let Some(path) = open_file_dialog() { open_or_reuse_tab(s, VPath::Local(path)); }
                            true
                        } else if (ctrl || cmd) && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "s") {
                            execute_command(s, CommandAction::Save);
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "=") {
                            let old = s.font_size;
                            s.font_size = (s.font_size + 2.0).min(36.0);
                            let ratio = s.font_size / old;
                            s.explorer_w = ((s.explorer_w as f32 * ratio).round() as i32).clamp(80, 600);
                            s.settings.font_size = s.font_size;
                            s.settings.save();
                            s.rebuild_glyphs();
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "-") {
                            let old = s.font_size;
                            s.font_size = (s.font_size - 2.0).max(8.0);
                            let ratio = s.font_size / old;
                            s.explorer_w = ((s.explorer_w as f32 * ratio).round() as i32).clamp(80, 600);
                            s.settings.font_size = s.font_size;
                            s.settings.save();
                            s.rebuild_glyphs();
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "0") {
                            let old = s.font_size;
                            s.font_size = FONT_PX;
                            let ratio = s.font_size / old;
                            s.explorer_w = ((s.explorer_w as f32 * ratio).round() as i32).clamp(80, 600);
                            s.settings.font_size = s.font_size;
                            s.settings.save();
                            s.rebuild_glyphs();
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
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "w") {
                            let pane_id = s.active_pane;
                            // Terminal panes are handled before PTY forwarding; only editor/lsp reach here
                            let pane = s.panes.get_mut(&pane_id).unwrap();
                            if pane.tabs.len() > 1 {
                                let closed = pane.tabs.remove(pane.active);
                                if let Some(ref p) = closed.path {
                                    let lang = Lang::from_path(p.as_path());
                                    if let Some(srv) = s.lsp.server_for_lang_mut(lang) {
                                        lsp::notify_did_close(srv, p);
                                    }
                                }
                                if pane.active >= pane.tabs.len() { pane.active = pane.tabs.len() - 1; }
                            } else if s.panes.len() > 1 {
                                s.panes.remove(&pane_id);
                                let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                                if let Some(new_tree) = remove_pane_from_tree(old_tree, pane_id) {
                                    s.pane_tree = new_tree;
                                }
                                let new_active = layout_tree(&s.pane_tree, s.pane_area())
                                    .first().map(|(id, _)| *id).unwrap_or(0);
                                s.active_pane = new_active;
                            } else {
                                s.panes.clear();
                                el.exit();
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
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "]") {
                            if s.pane().kind == PaneKind::Editor && !s.pane().tabs.is_empty() {
                                let pane = s.pane_mut();
                                pane.active = (pane.active + 1) % pane.tabs.len();
                            }
                            true
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "[") {
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
                        }
                        // ── Find / Replace ────────────────────────────────────────────────────
                        else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "f") {
                            s.find_mut().open         = true;
                            s.find_mut().replace_open = false;
                            s.find_mut().focus        = FindFocus::Query;
                            s.find_mut().query.clear();
                            if let Some(t) = s.tab().sel_text() { s.find_mut().query = t; }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "h") {
                            s.find_mut().open         = true;
                            s.find_mut().replace_open = true;
                            s.find_mut().focus        = FindFocus::Query;
                            s.find_mut().query.clear();
                            if let Some(t) = s.tab().sel_text() { s.find_mut().query = t; }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "g") {
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
                                let (cs, ww) = { let f = s.find(); (f.case_sensitive, f.whole_word) };
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
                                let (cs, ww) = { let f = s.find(); (f.case_sensitive, f.whole_word) };
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
                                s.find_mut().open = false;
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
                        } else if active_tab_is_settings {
                            match &event.logical_key {
                                Key::Named(NamedKey::ArrowUp) | Key::Named(NamedKey::PageUp) => {
                                    s.tab_mut().scroll = s.tab().scroll.saturating_sub(if matches!(&event.logical_key, Key::Named(NamedKey::PageUp)) { 5 } else { 1 });
                                }
                                Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::PageDown) => {
                                    let lh = s.glyphs.lh as usize;
                                    let pane_rect = s.active_pane_rect();
                                    let visible_h = (pane_rect.h - s.tab_h()).max(0) as usize;
                                    let content_h = if s.settings.undo_limit.is_some() { 24 * lh + 104 } else { 23 * lh + 100 };
                                    let max_scroll = content_h.saturating_sub(visible_h) / lh + 1;
                                    let step = if matches!(&event.logical_key, Key::Named(NamedKey::PageDown)) { 5 } else { 1 };
                                    s.tab_mut().scroll = (s.tab().scroll + step).min(max_scroll);
                                }
                                _ => {}
                            }
                            false
                        } else {
                            match &event.logical_key {
                                Key::Named(NamedKey::ArrowLeft) => {
                                    if cmd            { s.move_home(shift); }
                                    else if alt||ctrl { s.move_word_left(shift); }
                                    else              { s.move_left(shift); }
                                }
                                Key::Named(NamedKey::ArrowRight) => {
                                    if cmd            { s.move_end(shift); }
                                    else if alt||ctrl { s.move_word_right(shift); }
                                    else              { s.move_right(shift); }
                                }
                                Key::Named(NamedKey::ArrowUp) => {
                                    if cmd && alt      { s.add_cursor_above(); }
                                    else if cmd        { s.move_doc_start(shift); }
                                    else               { s.move_up(shift); }
                                }
                                Key::Named(NamedKey::ArrowDown) => {
                                    if cmd && alt      { s.add_cursor_below(); }
                                    else if cmd        { s.move_doc_end(shift); }
                                    else               { s.move_down(shift); }
                                }
                                Key::Named(NamedKey::Home) => {
                                    if ctrl { s.move_doc_start(shift); } else { s.move_home(shift); }
                                }
                                Key::Named(NamedKey::End) => {
                                    if ctrl { s.move_doc_end(shift); } else { s.move_end(shift); }
                                }
                                Key::Named(NamedKey::Backspace) => {
                                    if cmd            { s.delete_to_line_start(); }
                                    else if alt||ctrl { s.delete_word_back(); }
                                    else              { s.backspace(); }
                                }
                                Key::Named(NamedKey::Delete) => {
                                    if cmd            { s.delete_to_line_end(); }
                                    else if alt||ctrl { s.delete_word_fwd(); }
                                    else              { s.delete_fwd(); }
                                }
                                Key::Named(NamedKey::Enter)    => s.insert_str("\n"),
                                Key::Named(NamedKey::Tab)      => s.insert_str("    "),
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
                                    if !ctrl && !cmd {
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

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let Some(s) = self.state.as_mut() {
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

    fn user_event(&mut self, _el: &ActiveEventLoop, event: UserEvent) {
        let Some(s) = self.state.as_mut() else { return };
        if s.panes.is_empty() { return; }
        match event {
            UserEvent::TermOutput { pane_id, data } => {
                if let Some(tp) = s.term_panes.get_mut(&pane_id) {
                    terminal::feed_bytes(tp, &data);
                }
            }
            UserEvent::LspOutput { pane_id, data } => {
                if let Some(op) = s.lsp_panes.get_mut(&pane_id) {
                    let text = String::from_utf8_lossy(&data);
                    op.lines.extend(text.lines().map(String::from));
                }
                // Send `initialized` notification after receiving initialize response
                if let Some(srv) = s.lsp.servers.get_mut(&pane_id) {
                    if !srv.initialized {
                        if let Ok(text) = String::from_utf8(data) {
                            if lsp::is_initialize_response(&text) {
                                lsp::send_initialized(srv);
                            }
                        }
                    }
                }
            }
            UserEvent::LspDiagnostics { path, diagnostics } => {
                s.diagnostics.insert(path, diagnostics);
            }
            UserEvent::LspResponse { server_id, id, result } => {
                let kind = s.lsp.servers.get_mut(&server_id)
                    .and_then(|srv| srv.pending.remove(&id));
                match kind {
                    Some(lsp::PendingKind::Definition) => {
                        apply_goto_definition(s, &result);
                    }
                    Some(lsp::PendingKind::References) => {
                        apply_references(s, &result);
                    }
                    Some(lsp::PendingKind::Formatting { path }) => {
                        apply_text_edits(s, &path, &result);
                    }
                    Some(lsp::PendingKind::OrganizeImports { path }) => {
                        apply_organize_imports(s, &path, &result);
                    }
                    None => {}
                }
                s.needs_redraw = true;
            }
            UserEvent::FormatterDone { path } => {
                for pane in s.panes.values_mut() {
                    for tab in pane.tabs.iter_mut() {
                        if tab.path.as_ref() == Some(&path) {
                            if let Some(local) = path.as_local_path() {
                                if let Ok(text) = std::fs::read_to_string(local) {
                                    tab.text = ropey::Rope::from_str(&text);
                                    tab.dirty = false;
                                    tab.hl_dirty_from = 0;
                                    tab.hl_color_cache.clear();
                                    tab.max_line_len = None;
                                    tab.edit_generation += 1;
                                }
                            }
                        }
                    }
                }
                if s.left_view == LeftView::Git && s.left_panel_visible {
                    let root = s.explorer.as_ref()
                        .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    s.git_panel.loading = true;
                    refresh_git_status(s.proxy.clone(), root);
                }
                s.needs_redraw = true;
            }
            UserEvent::GitStatusResult { staged, unstaged, is_git_repo } => {
                s.git_panel.staged      = staged;
                s.git_panel.unstaged    = unstaged;
                s.git_panel.is_git_repo = is_git_repo;
                s.git_panel.loading     = false;
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
                if token == s.quick_finder.walk_token {
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
                let root = s.explorer.as_ref()
                    .and_then(|e| e.root.as_local_path().map(|p| p.to_path_buf()))
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                s.git_panel.loading = true;
                refresh_git_status(s.proxy.clone(), root);
                s.needs_redraw = true;
            }
            UserEvent::Redraw => {}

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
                // Trigger loading of any remote tabs that are waiting for this host.
                let pending_remote: Vec<VPath> = s.panes.values()
                    .flat_map(|p| p.tabs.iter())
                    .filter_map(|t| {
                        let path = t.path.as_ref()?;
                        if path.ssh_host() == Some(&host) && t.text.len_chars() == 0 {
                            Some(path.clone())
                        } else { None }
                    })
                    .collect();
                let token_base = s.next_buf_id as u64;
                for (i, vpath) in pending_remote.into_iter().enumerate() {
                    if let VPath::Remote { host: h, path: rpath } = vpath {
                        ssh::ssh_read_file(h, rpath, token_base + i as u64, s.proxy.clone());
                    }
                }
                // Trigger dir listing for remote explorer root on this host.
                if let Some(ex) = &s.explorer {
                    if ex.root.ssh_host() == Some(&host) {
                        if let VPath::Remote { host: h, path: rpath } = ex.root.clone() {
                            ssh::ssh_list_dir(h, rpath, s.proxy.clone());
                        }
                    }
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
            UserEvent::RemoteFileContent { token: _, path, content } => {
                // Deliver content to the tab that was opened for this path.
                for pane in s.panes.values_mut() {
                    for tab in pane.tabs.iter_mut() {
                        if tab.path.as_ref() == Some(&path) && tab.text.len_chars() == 0 {
                            tab.text = ropey::Rope::from_str(&content);
                            tab.dirty = false;
                            tab.hl_dirty_from = 0;
                            tab.hl_color_cache.clear();
                            tab.max_line_len = None;
                            tab.edit_generation += 1;
                        }
                    }
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteWriteDone { path } => {
                // Mark the tab clean after a successful remote write.
                for pane in s.panes.values_mut() {
                    for tab in pane.tabs.iter_mut() {
                        if tab.path.as_ref() == Some(&path) {
                            tab.dirty = false;
                        }
                    }
                }
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::RemoteDirListing { path: _, entries: _ } => {
                // Phase 3: populate remote explorer entries when async listing arrives.
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }

            // ── Collab events ─────────────────────────────────────────────────
            UserEvent::CollabMessage { from_site_id, msg } => {
                handle_collab_msg(s, from_site_id, msg);
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabConnected { session, doc_text, peers: _ } => {
                // Guest: replace active tab's content with the host's current snapshot
                let ap = s.active_pane;
                if let Some(pane) = s.panes.get_mut(&ap) {
                    if pane.kind == PaneKind::Editor {
                        if let Some(tab) = pane.tabs.get_mut(pane.active) {
                            tab.text = ropey::Rope::from_str(&doc_text);
                            tab.dirty = false;
                            tab.hl_dirty_from = 0;
                            tab.hl_color_cache.clear();
                            tab.edit_generation += 1;
                            tab.max_line_len = None;
                            // Store the current tab path in the session
                            let path_str = tab.path.as_ref()
                                .map(|p| p.to_string())
                                .unwrap_or_default();
                            let mut sess = *session;
                            sess.doc_path = path_str;
                            s.collab = Some(sess);
                        }
                    }
                }
                let n = s.collab.as_ref().map_or(0, |sess| sess.peers.len());
                s.status_msg = Some(format!("🔒 collab: connected ({n} peer{})", if n == 1 { "" } else { "s" }));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabDisconnected => {
                s.collab = None;
                s.collab_before = None;
                s.status_msg = Some("Collab session ended".to_owned());
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabError { msg } => {
                s.status_msg = Some(format!("Collab error: {msg}"));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabGuestJoined { site_id, peer } => {
                // Host: send Welcome with current doc text, broadcast PeerJoined
                let (server_clock, _session_key, all_peers) = {
                    let Some(session) = s.collab.as_ref() else { return };
                    (session.server_clock, session.session_key, session.peers.clone())
                };
                let name = peer.name.clone();
                let color = peer.color;

                // Get current doc text from active tab
                let ap = s.active_pane;
                let doc_text = s.panes.get(&ap)
                    .filter(|p| p.kind == PaneKind::Editor)
                    .and_then(|p| p.tabs.get(p.active))
                    .map(|t| t.text.to_string())
                    .unwrap_or_default();

                let welcome = collab::CollabMsg::Welcome {
                    your_site_id: site_id,
                    doc_text,
                    server_clock,
                    peers: all_peers,
                };
                let new_peer = collab::PeerInfo { site_id, name: name.clone(), color };
                let peer_joined = collab::CollabMsg::PeerJoined { peer: new_peer.clone() };

                if let Some(session) = s.collab.as_ref() {
                    session.send_to_site(site_id, &welcome);
                    session.broadcast_except_msg(Some(site_id), &peer_joined);
                }
                if let Some(session) = s.collab.as_mut() {
                    session.peers.push(new_peer);
                }
                s.status_msg = Some(format!("{name} joined the session"));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
            UserEvent::CollabGuestLeft { site_id } => {
                let name = s.collab.as_ref()
                    .and_then(|sess| sess.peers.iter().find(|p| p.site_id == site_id))
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| format!("Guest {site_id}"));
                if let Some(session) = s.collab.as_mut() {
                    session.peers.retain(|p| p.site_id != site_id);
                    session.remote_cursors.remove(&site_id);
                    // Broadcast PeerLeft to remaining guests
                    session.broadcast_except_msg(None, &collab::CollabMsg::PeerLeft { site_id });
                }
                s.status_msg = Some(format!("{name} left the session"));
                s.needs_redraw = true;
                self.dirty.store(true, Ordering::Release);
            }
        }
        s.win.request_redraw();
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
            let buf_id = s.panes[&ap].tabs[at].buf_id;
            let text   = s.panes[&ap].tabs[at].text.clone();
            let path   = s.panes[&ap].tabs[at].path.clone();
            let dirty  = s.panes[&ap].tabs[at].dirty;
            for (pid, p) in s.panes.iter_mut() {
                for (tidx, t) in p.tabs.iter_mut().enumerate() {
                    if t.buf_id == buf_id && (*pid != ap || tidx != at) {
                        t.text  = text.clone();
                        t.path  = path.clone();
                        t.dirty = dirty;
                    }
                }
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

    // Build LSP output pane snapshots
    struct OutPaneSnap {
        rect:          Rect,
        is_active:     bool,
        visible_lines: Vec<String>,
        title:         String,
        total_lines:   usize,
        scroll:        usize,
    }
    let out_snaps: Vec<OutPaneSnap> = layout.iter().filter_map(|&(pid, rect)| {
        let pane = s.panes.get(&pid)?;
        if pane.kind != PaneKind::LspOutput { return None; }
        let op = s.lsp_panes.get(&pid)?;
        let content_h = (rect.h - tab_h).max(0);
        let vis = (content_h / lh).max(1) as usize;
        let scroll = op.scroll;
        let start = scroll.min(op.lines.len());
        let visible_lines: Vec<String> = op.lines[start..].iter().take(vis).cloned().collect();
        Some(OutPaneSnap {
            rect,
            is_active: pid == active_pane_id,
            visible_lines,
            title: op.title.clone(),
            total_lines: op.lines.len(),
            scroll,
        })
    }).collect();

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
        let lang = tab.path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);
        let fh  = if pane.find.open { lh + 4 + if pane.find.replace_open { lh + 4 } else { 0 } } else { 0 };
        let vis = ((rect.h - tab_h - fh).max(0) / lh).max(1) as usize;
        let scroll = tab.scroll;
        let total  = tab.text.len_lines();

        // Cache max_line_len on demand
        if tab.max_line_len.is_none() {
            tab.max_line_len = Some((0..total).map(|li| State::line_len(&tab.text, li)).max().unwrap_or(0));
        }

        // Rebuild hl_cache from hl_dirty_from up through scroll + vis
        if lang != Lang::None {
            let need_up_to = (scroll + vis).min(total);
            let current_len = tab.hl_cache.len();
            if current_len < total { tab.hl_cache.resize(total, (MlState::Normal, 0)); }
            if tab.hl_dirty_from < need_up_to {
                let (start_state, start_depth) = if tab.hl_dirty_from == 0 {
                    (MlState::Normal, 0i32)
                } else {
                    tab.hl_cache.get(tab.hl_dirty_from - 1).copied().unwrap_or((MlState::Normal, 0))
                };
                let mut state = start_state;
                let mut depth = start_depth;
                if tab.hl_color_cache.len() < need_up_to {
                    tab.hl_color_cache.resize(need_up_to, Vec::new());
                }
                for li in tab.hl_dirty_from..need_up_to {
                    tab.hl_cache[li] = (state, depth);
                    let chars: Vec<char> = tab.text.line(li)
                        .chars().take_while(|&c| c != '\n' && c != '\r').collect();
                    let (colors, ns, nd) = highlight_line(&chars, lang, state, rainbow, depth);
                    state = ns;
                    depth = nd;
                    tab.hl_color_cache[li] = colors;
                }
                tab.hl_dirty_from = need_up_to;
            }
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
                find_open: false, find_repl_open: false,
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
                find_open: false, find_repl_open: false,
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
        let lang    = tab.path.as_ref().map(|p| Lang::from_path(p.as_path())).unwrap_or(Lang::None);

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
            let colors = if let Some(cached) = tab.hl_color_cache.get(li).filter(|v| !v.is_empty()) {
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

        // Remote collab cursors — convert char offsets to (line, col, color)
        let remote_cursors: Vec<(usize, usize, u32)> = s.collab.as_ref().map_or(vec![], |session| {
            session.remote_cursors.iter().flat_map(|(&sid, cursors)| {
                let color = session.peers.iter()
                    .find(|p| p.site_id == sid)
                    .map_or(0xFF6B6B, |p| p.color);
                cursors.iter().map(move |rc| {
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
            find_focus: pane.find.focus, case_sensitive: pane.find.case_sensitive,
            whole_word: pane.find.whole_word,
            find_query: fq, find_repl: pane.find.replace.clone(),
            find_cursor_q: pane.find.query[..pane.find.cursor_query].chars().count(),
            find_cursor_r: pane.find.replace[..pane.find.cursor_replace].chars().count(),
            find_sel_q: pane.find.sel_anchor_q.map(|a| pane.find.query[..a].chars().count()),
            find_sel_r: pane.find.sel_anchor_r.map(|a| pane.find.replace[..a].chars().count()),
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
    let ts_installed     = s.lsp_installed.get(&Lang::TypeScript).copied().unwrap_or(false);
    let ts_running       = s.lsp.has_server_for(Lang::TypeScript);
    let rust_installed   = s.lsp_installed.get(&Lang::Rust).copied().unwrap_or(false);
    let rust_running     = s.lsp.has_server_for(Lang::Rust);
    let py_installed     = s.lsp_installed.get(&Lang::Python).copied().unwrap_or(false);
    let py_running       = s.lsp.has_server_for(Lang::Python);
    let format_on_save          = s.settings.format_on_save.clone();
    let organize_imports_on_save = s.settings.organize_imports_on_save.clone();
    let format_command          = s.settings.format_command.clone();
    let glyph_cache_limit       = s.settings.glyph_cache_limit;
    let cpu_double_buffer       = s.settings.cpu_double_buffer;
    let gpu_drawable_count      = s.settings.gpu_drawable_count;
    let settings_edit_field     = s.settings_edit_field;
    let settings_edit_text      = s.settings_edit_text.clone();
    let settings_edit_cursor    = s.settings_edit_cursor;
    let explorer_drag    = s.explorer_drag;
    let ui_scale         = s.font_size / FONT_PX;
    let left_view        = s.left_view;
    let left_panel_visible = s.left_panel_visible;
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
            collab::CollabRole::Guest => {
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
    let qf_selected     = s.quick_finder.selected;
    let qf_items: Vec<(String, String)> = if qf_open && !qf_is_cmd_mode {
        let n = s.quick_finder.filtered.len();
        let view_start = qf_selected.saturating_sub(4).min(n.saturating_sub(10));
        let view_end   = (view_start + 10).min(n);
        s.quick_finder.filtered[view_start..view_end].iter().map(|&idx| {
            let p = &s.quick_finder.entries[idx];
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
            let dir  = p.parent_str();
            (name, dir)
        }).collect()
    } else { vec![] };
    let qf_sel_in_view = if qf_open && !qf_is_cmd_mode {
        let n = s.quick_finder.filtered.len();
        let view_start = qf_selected.saturating_sub(4).min(n.saturating_sub(10));
        qf_selected.saturating_sub(view_start)
    } else { 0 };
    let qf_cmd_items: Vec<(String, String)> = if qf_open && qf_is_cmd_mode {
        let n = s.quick_finder.filtered_commands.len();
        let view_start = qf_selected.saturating_sub(4).min(n.saturating_sub(10));
        let view_end   = (view_start + 10).min(n);
        s.quick_finder.filtered_commands[view_start..view_end].iter().map(|&idx| {
            (COMMANDS[idx].name.to_owned(), COMMANDS[idx].shortcut.to_owned())
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
    let cmd_held = s.mods.super_key();
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

            let git_icon_y  = diag_icon_y + icon_size + 4;

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
                        let ey = entries_start + i as i32 * lh;
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
                    if is_sel { fill(buf, w, h, px, ry, pw - 1, lh, SEL_BG); }
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
                    if is_sel { fill(buf, w, h, px, ry, pw - 1, lh, SEL_BG); }
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
                    draw_str(buf, w, h, g, " Not a git repo", px + 2, asc + 4, FG_DIM, px + pw - 1);
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
                    let scale_str = format!("  {:.0}%  (Cmd+= / Cmd+-)", ui_scale * 100.0);
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
                    draw_str(buf, w, h, g, "  Copy/Paste (Cmd+C / Cmd+V)", r.x, tcp_y + asc, FG, btn_x - cw);
                    let (tcp_bg, tcp_fg) = if term_copy_paste { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                    fc(buf, btn_x, tcp_y, 8 * cw, lh, tcp_bg);
                    draw_str(buf, w, h, g, if term_copy_paste { " [x] On " } else { " [ ] Off" }, btn_x, tcp_y + asc, tcp_fg, btn_x + 8 * cw);
                }

                let tcb_y = tcp_y + lh + 4;
                if row_vis(tcb_y) {
                    draw_str(buf, w, h, g, "  Cmd+Backspace \u{2192} \\x15", r.x, tcb_y + asc, FG, btn_x - cw);
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
                    draw_str(buf, w, h, g, "  Language Servers", r.x, lsp_sec_y + asc, FG, btn_x - cw);
                }
                fc(buf, r.x + cw, lsp_sec_y + lh - 1, r.w - 2 * cw, 1, BORDER);
                let inst_btn_w = 9 * cw;

                let lsp_rows: [(&str, bool, bool, &str); 3] = [
                    ("TypeScript", ts_installed, ts_running, "npm i -g typescript-language-server typescript"),
                    ("Rust      ", rust_installed, rust_running, "rustup component add rust-analyzer"),
                    ("Python    ", py_installed, py_running, "pip install python-lsp-server"),
                ];
                for (i, (name, installed, running, _)) in lsp_rows.iter().enumerate() {
                    let ly = lsp_sec_y + lh + 4 + i as i32 * (lh + 4);
                    if !row_vis(ly) { continue; }
                    draw_str(buf, w, h, g, &format!("  {}", name), r.x, ly + asc, FG, btn_x - cw);
                    if *running {
                        fc(buf, btn_x, ly, 10 * cw, lh, ACCENT);
                        draw_str(buf, w, h, g, " running  ", btn_x, ly + asc, BG, btn_x + 10 * cw);
                    } else if *installed {
                        draw_str(buf, w, h, g, " installed", btn_x, ly + asc, FG_DIM, btn_x + 10 * cw);
                        fc(buf, btn_x + 11 * cw, ly, inst_btn_w, lh, SEL_BG);
                        draw_str(buf, w, h, g, "[Install]", btn_x + 11 * cw, ly + asc, FG, btn_x + 11 * cw + inst_btn_w);
                    } else {
                        draw_str(buf, w, h, g, " not found", btn_x, ly + asc, FG_DIM, btn_x + 10 * cw);
                        fc(buf, btn_x + 11 * cw, ly, inst_btn_w, lh, SEL_BG);
                        draw_str(buf, w, h, g, "[Install]", btn_x + 11 * cw, ly + asc, FG, btn_x + 11 * cw + inst_btn_w);
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
                if snap.find_focus == FindFocus::Query {
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
                    if snap.find_focus == FindFocus::Replace {
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
                fill(buf, w, h, cx, cy, 2, lh, ACCENT);
            }
        }

        // ── LSP output panes ──────────────────────────────────────────────
        for snap in &out_snaps {
            let r = snap.rect;
            // Tab bar
            fill(buf, w, h, r.x, r.y, r.w, tab_h, BG2);
            fill(buf, w, h, r.x, r.y + tab_h - 1, r.w, 1, BORDER);
            if snap.is_active { fill(buf, w, h, r.x, r.y, 2, tab_h - 1, ACCENT); }
            draw_str(buf, w, h, g, &format!(" {}", snap.title), r.x + 4, r.y + tab_h * 3 / 4, FG_DIM, r.x + r.w);
            // Lines
            for (vi, line) in snap.visible_lines.iter().enumerate() {
                let py       = r.y + tab_h + vi as i32 * lh;
                let baseline = py + asc;
                draw_str(buf, w, h, g, line, r.x + 4, baseline, FG_DIM, r.x + r.w);
            }
            // Scrollbar
            if snap.total_lines > 0 {
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
        for snap in &out_snaps {
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
            let sc_w    = items.iter().filter(|i| i.action != CtxAction::Separator).map(|i| i.shortcut.chars().count()).max().unwrap_or(0) as i32;
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
                    let sc_x = menu_x + menu_w - item.shortcut.chars().count() as i32 * cw - cw;
                    draw_str(buf, w, h, g, item.shortcut, sc_x, iy + asc, FG_DIM, menu_x + menu_w - 1);
                }
                iy += item_h;
            }
        }

        // ── Quick finder / command palette overlay (unified) ─────────────
        if qf_open {
            darken_buffer(buf, w, h);
            let item_count = if qf_is_cmd_mode { qf_cmd_items.len() } else { qf_items.len().max(if qf_loading { 1 } else { 0 }) };
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
            } else if qf_loading {
                let ry = oy + 4 + lh + 4;
                draw_str(buf, w, h, g, "Searching files...", ox + 6, ry + asc, FG_DIM, ox + ow - 4);
            } else {
                let avail_chars = ((ow - 10) / cw).max(4) as usize;
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
                VPath::Local(p) if p.is_dir() => (None, Some(vp)),
                VPath::Remote { .. }           => (None, Some(vp)),
                _                             => (Some(vp), None),
            }
        }
        None => (None, None),
    };
    let el = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    let proxy = el.create_proxy();
    let mut app = App::new(file_arg, dir_arg, proxy);
    el.run_app(&mut app).unwrap();
}
