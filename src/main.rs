mod platform;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use fontdue::{Font, FontSettings, Metrics};
use ropey::Rope;
use winit::application::ApplicationHandler;
use std::time::{Duration, Instant};
#[cfg(feature = "logging")]
use std::time::{SystemTime, UNIX_EPOCH};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowAttributes};
#[cfg(target_os = "macos")]
use winit::platform::macos::WindowAttributesExtMacOS;

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

// ── Language detection ────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq)]
enum Lang { None, Rust, Python, TypeScript }

impl Lang {
    fn from_path(path: &std::path::Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs")                          => Lang::Rust,
            Some("py" | "pyw")                  => Lang::Python,
            Some("ts" | "tsx" | "js" | "jsx")   => Lang::TypeScript,
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
}

// ── Layout ────────────────────────────────────────────────────────────────────
const FONT_PX:    f32 = 14.0;
const EXPLORER_W: i32 = 200;
const ED_LPAD:    i32 = 6;
const SB_W:       i32 = 6;
const SB_THUMB:   u32 = 0x414868;

// ── Glyph cache ───────────────────────────────────────────────────────────────
struct Glyphs {
    font: Font,
    px:   f32,
    map:  HashMap<char, (Metrics, Vec<u8>)>,
    pub cw: i32,
    pub lh: i32,
    pub asc: i32,
}

impl Glyphs {
    fn new(bytes: &[u8], px: f32) -> Self {
        let font = Font::from_bytes(bytes, FontSettings::default()).unwrap();
        let mut s = Self { font, px, map: HashMap::new(), cw: 0, lh: 0, asc: 0 };
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
        for ch in ['▶', '▼', '•'] { self.load(ch); }
    }

    fn load(&mut self, ch: char) {
        self.map.entry(ch).or_insert_with(|| self.font.rasterize(ch, self.px));
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
}

impl FindBar {
    fn new() -> Self {
        FindBar {
            open: false, replace_open: false,
            query: String::new(), replace: String::new(),
            case_sensitive: false, whole_word: false,
            focus: FindFocus::Query,
        }
    }
    fn active_field_mut(&mut self) -> &mut String {
        if self.focus == FindFocus::Query { &mut self.query } else { &mut self.replace }
    }
}

// ── Tab (per-file state) ──────────────────────────────────────────────────────
struct Tab {
    text:    Rope,
    path:    Option<PathBuf>,
    dirty:   bool,
    cursors: Vec<Cursor>, // always non-empty; primary = last element
    scroll:  usize,
    hscroll: usize,
}

impl Tab {
    fn untitled() -> Self {
        Tab { text: Rope::new(), path: None, dirty: false,
              cursors: vec![Cursor::new(0)], scroll: 0, hscroll: 0 }
    }

    fn display_name(&self) -> &str {
        self.path.as_deref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled")
    }

    fn is_empty_untitled(&self) -> bool {
        self.path.is_none() && self.text.len_chars() == 0
    }

    fn primary(&self) -> &Cursor { self.cursors.last().unwrap() }
    fn primary_mut(&mut self) -> &mut Cursor { self.cursors.last_mut().unwrap() }

    fn sel(&self) -> Option<(usize, usize)> { self.primary().sel() }

    fn sel_text(&self) -> Option<String> {
        self.sel().map(|(lo, hi)| self.text.slice(lo..hi).chars().collect())
    }

    fn load_file(&mut self, path: PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                self.text    = Rope::from_str(&content);
                self.path    = Some(path);
                self.cursors = vec![Cursor::new(0)];
                self.scroll  = 0;
                self.hscroll = 0;
                self.dirty   = false;
            }
            Err(e) => eprintln!("open error: {e}"),
        }
    }

    fn save(&mut self) {
        let Some(path) = &self.path else { return };
        let content: String = self.text.chunks().collect();
        match std::fs::write(path, content) {
            Ok(_)  => self.dirty = false,
            Err(e) => eprintln!("save error: {e}"),
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
    root:        PathBuf,
    entries:     Vec<FileEntry>,
    selected:    usize,
    show_hidden: bool,
}

impl FileExplorer {
    fn new(root: PathBuf) -> Self {
        let entries = load_dir_entries(&root, 0, false);
        FileExplorer { root, entries, selected: 0, show_hidden: false }
    }

    fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.entries = load_dir_entries(&self.root, 0, self.show_hidden);
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
            let children = load_dir_entries(&path, depth, self.show_hidden);
            for (i, child) in children.into_iter().enumerate() {
                self.entries.insert(idx + 1 + i, child);
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }
}

fn load_dir_entries(dir: &PathBuf, depth: usize, show_hidden: bool) -> Vec<FileEntry> {
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

// ── Application state ─────────────────────────────────────────────────────────
struct State {
    win:      Arc<Window>,
    renderer: platform::Renderer,
    w: u32, h: u32,

    tabs:   Vec<Tab>,
    active: usize,

    cursor_visible: bool,
    cursor_blink:   Instant,
    mods:   ModifiersState,

    font_size: f32,
    glyphs:     Glyphs,

    explorer: Option<FileExplorer>,
    mouse_x:    f32,
    mouse_y:    f32,
    mouse_down:      bool,
    last_click_time: Instant,
    last_click_char: usize,
    click_count:     u32,

    find: FindBar,
}

impl State {
    fn tab(&self)         -> &Tab     { &self.tabs[self.active] }
    fn tab_mut(&mut self) -> &mut Tab { &mut self.tabs[self.active] }

    fn explorer_w(&self) -> i32 {
        if self.explorer.is_some() { EXPLORER_W } else { 0 }
    }

    fn tab_h(&self)    -> i32 { self.glyphs.lh + 4 }
    fn status_h(&self) -> i32 { self.glyphs.lh + 4 }
    fn find_h(&self)   -> i32 {
        if !self.find.open { return 0; }
        let row_h = self.glyphs.lh + 4;
        if self.find.replace_open { row_h * 2 } else { row_h }
    }
    fn editor_h(&self) -> i32 { self.h as i32 - self.tab_h() - self.status_h() - self.find_h() }

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
        let vis_v = (self.editor_h() / self.glyphs.lh).max(1) as usize;
        let vis_h = ((self.w as i32 - self.explorer_w() - ED_LPAD) / self.glyphs.cw).max(1) as usize;
        let (line, col) = self.cursor_lc();
        let t = self.tab_mut();
        if line < t.scroll              { t.scroll  = line; }
        if line >= t.scroll  + vis_v   { t.scroll  = line + 1 - vis_v; }
        if col  < t.hscroll             { t.hscroll = col; }
        if col  >= t.hscroll + vis_h   { t.hscroll = col + 1 - vis_h; }
    }

    // ── Multi-cursor helpers ──────────────────────────────────────────────────

    // Returns cursor indices sorted by lo() descending (right-to-left order).
    fn cursor_order_rtl(&self) -> Vec<usize> {
        let n = self.tab().cursors.len();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            self.tab().cursors[b].lo().cmp(&self.tab().cursors[a].lo())
        });
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

    // ── Editing ───────────────────────────────────────────────────────────────

    fn insert_str(&mut self, text: &str) {
        let n_chars = text.chars().count();
        let order = self.cursor_order_rtl();
        for &i in &order {
            let (has_sel, lo, hi) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi())
            };
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
            }
            let pos = self.tab().cursors[i].head.min(self.tab().text.len_chars());
            self.tab_mut().text.insert(pos, text);
            self.tab_mut().cursors[i] = Cursor::new(pos + n_chars);
            self.tab_mut().dirty = true;
        }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn backspace(&mut self) {
        let order = self.cursor_order_rtl();
        for &i in &order {
            let (has_sel, lo, hi, head) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi(), c.head)
            };
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                self.tab_mut().dirty = true;
            } else if head > 0 {
                let c = head.min(self.tab().text.len_chars());
                self.tab_mut().text.remove(c - 1..c);
                self.tab_mut().cursors[i] = Cursor::new(c - 1);
                self.tab_mut().dirty = true;
            }
        }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn delete_fwd(&mut self) {
        let order = self.cursor_order_rtl();
        for &i in &order {
            let (has_sel, lo, hi, head) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi(), c.head)
            };
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                self.tab_mut().dirty = true;
            } else {
                let c = head.min(self.tab().text.len_chars());
                if c < self.tab().text.len_chars() {
                    self.tab_mut().text.remove(c..c + 1);
                    self.tab_mut().dirty = true;
                }
            }
        }
        self.dedup_cursors();
    }

    fn delete_word_back(&mut self) {
        let order = self.cursor_order_rtl();
        for &i in &order {
            let (has_sel, lo, hi) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi())
            };
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                self.tab_mut().dirty = true;
            } else {
                let end = self.tab().cursors[i].head.min(self.tab().text.len_chars());
                let mut start = end;
                while start > 0 && !Self::is_word_char(self.tab().text.char(start - 1)) { start -= 1; }
                while start > 0 &&  Self::is_word_char(self.tab().text.char(start - 1)) { start -= 1; }
                if start < end {
                    self.tab_mut().text.remove(start..end);
                    self.tab_mut().cursors[i] = Cursor::new(start);
                    self.tab_mut().dirty = true;
                }
            }
        }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn delete_to_line_start(&mut self) {
        let order = self.cursor_order_rtl();
        for &i in &order {
            let (has_sel, lo, hi) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi())
            };
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                self.tab_mut().dirty = true;
            } else {
                let cursor_pos = self.tab().cursors[i].head.min(self.tab().text.len_chars());
                let line = self.tab().text.char_to_line(cursor_pos);
                let start = self.tab().text.line_to_char(line);
                if start < cursor_pos {
                    self.tab_mut().text.remove(start..cursor_pos);
                    self.tab_mut().cursors[i] = Cursor::new(start);
                    self.tab_mut().dirty = true;
                } else if start > 0 {
                    self.tab_mut().text.remove(start - 1..start);
                    self.tab_mut().cursors[i] = Cursor::new(start - 1);
                    self.tab_mut().dirty = true;
                }
            }
        }
        self.dedup_cursors();
        self.ensure_visible();
    }

    fn delete_word_fwd(&mut self) {
        let order = self.cursor_order_rtl();
        for &i in &order {
            let (has_sel, lo, hi) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi())
            };
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                self.tab_mut().dirty = true;
            } else {
                let len = self.tab().text.len_chars();
                let start = self.tab().cursors[i].head.min(len);
                let mut end = start;
                while end < len && !Self::is_word_char(self.tab().text.char(end)) { end += 1; }
                while end < len &&  Self::is_word_char(self.tab().text.char(end)) { end += 1; }
                if end > start {
                    self.tab_mut().text.remove(start..end);
                    self.tab_mut().dirty = true;
                }
            }
        }
        self.dedup_cursors();
    }

    fn delete_to_line_end(&mut self) {
        let order = self.cursor_order_rtl();
        for &i in &order {
            let (has_sel, lo, hi) = {
                let c = &self.tab().cursors[i];
                (c.has_sel(), c.lo(), c.hi())
            };
            if has_sel {
                self.tab_mut().text.remove(lo..hi);
                self.tab_mut().cursors[i] = Cursor::new(lo);
                self.tab_mut().dirty = true;
            } else {
                let len = self.tab().text.len_chars();
                let c = self.tab().cursors[i].head.min(len);
                let l = self.tab().text.char_to_line(c);
                let line_end = self.tab().text.line_to_char(l) + Self::line_len(&self.tab().text, l);
                if line_end > c {
                    self.tab_mut().text.remove(c..line_end);
                    self.tab_mut().dirty = true;
                } else if c < len {
                    self.tab_mut().text.remove(c..c + 1);
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
        let last = { let t = self.tab(); Self::last_line(&t.text) };
        let t = self.tab_mut();
        if delta < 0 {
            t.scroll = t.scroll.saturating_sub((-delta) as usize);
        } else {
            t.scroll = (t.scroll + delta as usize).min(last);
        }
    }

    fn hscroll_by(&mut self, delta: i32) {
        let vis_h = ((self.w as i32 - self.explorer_w() - ED_LPAD) / self.glyphs.cw).max(1) as usize;
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
        let tab_h = self.tab_h();
        let lh    = self.glyphs.lh;
        let cw    = self.glyphs.cw;
        let ed_x  = self.explorer_w() + ED_LPAD;
        let vi    = ((my - tab_h).max(0) / lh) as usize;
        let t     = self.tab();
        let li    = (t.scroll + vi).min(t.text.len_lines().saturating_sub(1));
        let col   = ((mx - ed_x).max(0) / cw) as usize + t.hscroll;
        t.text.line_to_char(li) + col.min(Self::line_len(&t.text, li))
    }
}

// ── Helper: open file in a tab ────────────────────────────────────────────────
fn open_or_reuse_tab(s: &mut State, path: PathBuf) {
    for i in 0..s.tabs.len() {
        if s.tabs[i].path.as_deref() == Some(path.as_path()) {
            s.active = i;
            return;
        }
    }
    if s.tab().is_empty_untitled() {
        s.tab_mut().load_file(path);
    } else {
        let mut tab = Tab::untitled();
        tab.load_file(path);
        s.tabs.push(tab);
        s.active = s.tabs.len() - 1;
    }
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
        Lang::None => false,
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
        Lang::None => false,
    }
}

fn classify_word(word: &str, lang: Lang, is_call: bool) -> u32 {
    if is_keyword(word, lang)  { return HL_KEYWORD; }
    if is_type_kw(word, lang)  { return HL_TYPE; }
    if word.chars().next().map_or(false, |c| c.is_uppercase()) { return HL_TYPE; }
    if is_call                 { return HL_FUNC; }
    FG
}

fn highlight_line(chars: &[char], lang: Lang, mut state: MlState) -> (Vec<u32>, MlState) {
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

                i += 1;
            }
        }
    }

    (out, state)
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
    let content: Vec<char> = text.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();
    let qlen = query_chars.len();
    let total = content.len();
    if qlen > total { return vec![]; }
    let mut out = vec![];
    let mut i = 0;
    while i + qlen <= total {
        let hit = content[i..i + qlen].iter().zip(query_chars.iter()).all(|(c, q)| {
            if case_sensitive { c == q }
            else { c.to_lowercase().eq(q.to_lowercase()) }
        });
        if hit {
            let ok = !whole_word || {
                let before = i == 0 || !State::is_word_char(content[i - 1]);
                let after  = i + qlen >= total || !State::is_word_char(content[i + qlen]);
                before && after
            };
            if ok { out.push((i, i + qlen)); }
        }
        i += 1;
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
    let matches = find_matches(&s.tab().text, &s.find.query, s.find.case_sensitive, s.find.whole_word);
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
        let repl = s.find.replace.clone();
        for ch in repl.chars() { s.glyphs.load(ch); }
        s.insert_str(&repl);
        find_step(s, false);
    }
}

fn replace_all(s: &mut State) {
    let matches = find_matches(&s.tab().text, &s.find.query, s.find.case_sensitive, s.find.whole_word);
    if matches.is_empty() { return; }
    let repl = s.find.replace.clone();
    for ch in repl.chars() { s.glyphs.load(ch); }
    for &(lo, hi) in matches.iter().rev() {
        s.tab_mut().text.remove(lo..hi);
        s.tab_mut().text.insert(lo, &repl);
    }
    s.tab_mut().dirty = true;
    s.tab_mut().cursors = vec![Cursor::new(0)];
    s.ensure_visible();
}

// ── App ───────────────────────────────────────────────────────────────────────
struct App {
    state:    Option<State>,
    file_arg: Option<PathBuf>,
    dir_arg:  Option<PathBuf>,
}

impl App {
    fn new(file_arg: Option<PathBuf>, dir_arg: Option<PathBuf>) -> Self {
        Self { state: None, file_arg, dir_arg }
    }
}

impl ApplicationHandler for App {
    fn new_events(&mut self, _el: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            if let Some(s) = self.state.as_mut() {
                dlog!("[blink] t={}", ts());
                s.cursor_visible = !s.cursor_visible;
                s.cursor_blink = Instant::now() + Duration::from_millis(500);
                s.win.request_redraw();
            }
        }
    }

    fn resumed(&mut self, el: &ActiveEventLoop) {
        let mut attrs = WindowAttributes::default().with_title("local-text");
        #[cfg(target_os = "macos")]
        { attrs = attrs.with_title_hidden(true).with_titlebar_transparent(true); }

        let win = Arc::new(el.create_window(attrs).unwrap());
        let renderer = platform::Renderer::new(win.clone());

        let font_size = FONT_PX;
        let glyphs = Glyphs::new(include_bytes!("../assets/JetBrainsMono-Regular.ttf"), font_size);

        let mut initial_tab = Tab::untitled();
        if let Some(path) = self.file_arg.take() {
            initial_tab.load_file(path);
        }

        let explorer = self.dir_arg.take().map(FileExplorer::new);
        let sz = win.inner_size();

        let s = State {
            win,
            renderer,
            w: sz.width,
            h: sz.height,
            tabs:   vec![initial_tab],
            active: 0,
            cursor_visible: true,
            cursor_blink:   Instant::now() + Duration::from_millis(500),
            mods:   ModifiersState::default(),
            font_size,
            glyphs,
            explorer,
            mouse_x:    0.0,
            mouse_y:    0.0,
            mouse_down:      false,
            last_click_time: Instant::now() - Duration::from_secs(1),
            last_click_char: usize::MAX,
            click_count:     0,
            find: FindBar::new(),
        };

        s.win.request_redraw();
        self.state = Some(s);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: winit::window::WindowId, event: WindowEvent) {
        let Some(s) = self.state.as_mut() else { return };

        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::Focused(_focused) => {
                dlog!("[focus] focused={_focused} t={}", ts());
            }

            WindowEvent::Resized(sz) => {
                dlog!("[resize] {}x{} -> {}x{}", s.w, s.h, sz.width, sz.height);
                s.w = sz.width;
                s.h = sz.height;
                s.renderer.resize(sz.width, sz.height);
                render(s);
            }

            WindowEvent::ModifiersChanged(m) => {
                s.mods = m.state();
            }

            WindowEvent::CursorMoved { position, .. } => {
                s.mouse_x = position.x as f32;
                s.mouse_y = position.y as f32;
                let mx = s.mouse_x as i32;
                let my = s.mouse_y as i32;
                let in_editor = my >= s.tab_h()
                    && my < s.h as i32 - s.status_h() - s.find_h()
                    && mx >= s.explorer_w();
                s.win.set_cursor(if in_editor { CursorIcon::Text } else { CursorIcon::Default });
                if s.mouse_down {
                    let pos = s.xy_to_char(mx, my);
                    s.tab_mut().primary_mut().head = pos;
                    s.ensure_visible();
                    render(s);
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left, ..
            } => {
                s.mouse_down = false;
            }

            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left, ..
            } => {
                let mx  = s.mouse_x as i32;
                let my  = s.mouse_y as i32;
                let alt = s.mods.alt_key();

                // Find bar click
                let in_find = s.find.open && {
                    let fy = s.h as i32 - s.status_h() - s.find_h();
                    my >= fy && my < fy + s.find_h()
                };
                if in_find {
                    let find_y    = s.h as i32 - s.status_h() - s.find_h();
                    let row_h     = s.glyphs.lh + 4;
                    let cw        = s.glyphs.cw;
                    let rel_row   = (my - find_y) / row_h;
                    if rel_row == 0 {
                        // [Aa] and [W] toggles on the right
                        let aa_len     = 4usize; // "[Aa]" or "[aa]"
                        let w_len      = 3usize; // "[W]" or "[w]"
                        let toggle_w   = (aa_len + 1 + w_len) as i32 * cw + 8;
                        let aa_x       = s.w as i32 - toggle_w;
                        let wl_x       = aa_x + (aa_len + 1) as i32 * cw;
                        if mx >= aa_x && mx < wl_x     { s.find.case_sensitive = !s.find.case_sensitive; }
                        else if mx >= wl_x             { s.find.whole_word     = !s.find.whole_word; }
                        else                           { s.find.focus = FindFocus::Query; }
                    } else if rel_row == 1 && s.find.replace_open {
                        s.find.focus = FindFocus::Replace;
                        let repl_len = 6usize; // "[Repl]"
                        let all_len  = 5usize; // "[All]"
                        let btn_w    = (repl_len + 1 + all_len) as i32 * cw + 8;
                        let btn_x    = s.w as i32 - btn_w;
                        let all_x    = btn_x + (repl_len + 1) as i32 * cw;
                        if mx >= btn_x && mx < all_x   { replace_current(s); }
                        else if mx >= all_x            { replace_all(s); }
                    }
                    render(s);
                } else if my < s.tab_h() {
                    // Tab bar
                    let cw = s.glyphs.cw;
                    let mut tx = 0i32;
                    for i in 0..s.tabs.len() {
                        let name_len    = s.tabs[i].display_name().chars().count();
                        let dirty       = s.tabs[i].dirty;
                        let label_chars = name_len + if dirty { 4 } else { 3 };
                        let tw          = label_chars as i32 * cw + 1;
                        if mx < tx + tw {
                            s.active = i;
                            render(s);
                            break;
                        }
                        tx += tw;
                    }
                } else if s.explorer.is_some() && mx < EXPLORER_W {
                    // Explorer
                    let lh  = s.glyphs.lh;
                    let row = (my - s.tab_h()) / lh;
                    if row == 0 {
                        if let Some(ex) = s.explorer.as_mut() { ex.toggle_hidden(); }
                    } else if row > 0 {
                        let idx = (row - 1) as usize;
                        let action = s.explorer.as_mut().and_then(|ex| {
                            if idx < ex.entries.len() {
                                ex.selected = idx;
                                if ex.entries[idx].is_dir {
                                    ex.toggle(idx);
                                    None
                                } else {
                                    Some(ex.entries[idx].path.clone())
                                }
                            } else { None }
                        });
                        if let Some(path) = action { open_or_reuse_tab(s, path); }
                    }
                    render(s);
                } else {
                    // Editor area
                    let pos = s.xy_to_char(mx, my);
                    if alt {
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
                                // Double-click: select word under cursor
                                let (lo, hi) = word_bounds_at(s.tab(), pos);
                                s.tab_mut().cursors = vec![Cursor { head: hi, tail: lo }];
                                s.mouse_down = false;
                            }
                            n if n >= 3 => {
                                // Triple-click: select whole line
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
                                // Single click: place cursor
                                s.tab_mut().cursors = vec![Cursor { head: pos, tail: pos }];
                                s.mouse_down = true;
                            }
                        }
                    }
                    s.reset_blink();
                    render(s);
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x as i32, -y as i32),
                    MouseScrollDelta::PixelDelta(p)   => {
                        let cw = s.glyphs.cw;
                        let lh = s.glyphs.lh;
                        (-(p.x as i32) / cw, -(p.y as i32) / lh)
                    }
                };
                if dy != 0 { s.scroll_by(dy); }
                if dx != 0 { s.hscroll_by(dx); }
                if dx != 0 || dy != 0 { render(s); }
            }

            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                dlog!("[input] {:?}", event.logical_key);
                let ctrl  = s.mods.control_key();
                let cmd   = s.mods.super_key();
                let alt   = s.mods.alt_key();
                let shift = s.mods.shift_key();

                // Find bar: route non-cmd keys when open
                if s.find.open && !cmd {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => { s.find.open = false; }
                        Key::Named(NamedKey::Tab) => {
                            if s.find.replace_open {
                                s.find.focus = if s.find.focus == FindFocus::Query {
                                    FindFocus::Replace
                                } else {
                                    FindFocus::Query
                                };
                            }
                        }
                        Key::Named(NamedKey::Enter) => {
                            if s.find.focus == FindFocus::Replace && s.find.replace_open {
                                replace_current(s);
                            } else {
                                find_step(s, shift);
                            }
                        }
                        Key::Named(NamedKey::Backspace) => { s.find.active_field_mut().pop(); }
                        _ => {
                            if let Some(txt) = event.text.as_deref() {
                                s.find.active_field_mut().push_str(txt);
                            }
                        }
                    }
                    render(s);
                } else {
                    // Main editor keyboard handling
                    let handled =
                        if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "a") {
                            let len = s.tab().text.len_chars();
                            s.tab_mut().cursors = vec![Cursor { head: len, tail: 0 }];
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "c") {
                            if let Some(text) = s.tab().sel_text() { clipboard_set(&text); }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "x") {
                            if let Some(text) = s.tab().sel_text() {
                                clipboard_set(&text);
                                let order = s.cursor_order_rtl();
                                for &i in &order {
                                    if s.tab().cursors[i].has_sel() {
                                        let lo = s.tab().cursors[i].lo();
                                        let hi = s.tab().cursors[i].hi();
                                        s.tab_mut().text.remove(lo..hi);
                                        s.tab_mut().cursors[i] = Cursor::new(lo);
                                        s.tab_mut().dirty = true;
                                    }
                                }
                                s.dedup_cursors();
                                s.ensure_visible();
                            }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "v") {
                            if let Some(text) = clipboard_get() {
                                for ch in text.chars() { s.glyphs.load(ch); }
                                s.insert_str(&text);
                            }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "o") {
                            if let Some(path) = open_file_dialog() { open_or_reuse_tab(s, path); }
                            true
                        } else if (ctrl || cmd) && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "s") {
                            s.tab_mut().save();
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "=") {
                            s.font_size = (s.font_size + 2.0).min(36.0);
                            s.rebuild_glyphs();
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "-") {
                            s.font_size = (s.font_size - 2.0).max(8.0);
                            s.rebuild_glyphs();
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "0") {
                            s.font_size = FONT_PX;
                            s.rebuild_glyphs();
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "t") {
                            s.tabs.push(Tab::untitled());
                            s.active = s.tabs.len() - 1;
                            s.reset_blink();
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "w") {
                            if s.tabs.len() == 1 { el.exit(); }
                            else {
                                s.tabs.remove(s.active);
                                if s.active >= s.tabs.len() { s.active = s.tabs.len() - 1; }
                            }
                            true
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "]") {
                            s.active = (s.active + 1) % s.tabs.len();
                            true
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "[") {
                            s.active = s.active.checked_sub(1).unwrap_or(s.tabs.len() - 1);
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if matches!(c.as_str(), "1"|"2"|"3"|"4"|"5"|"6"|"7"|"8"|"9")) {
                            if let Key::Character(c) = &event.logical_key {
                                if let Ok(n) = c.as_str().parse::<usize>() {
                                    if n >= 1 && n - 1 < s.tabs.len() { s.active = n - 1; }
                                }
                            }
                            true
                        } else if ctrl && matches!(&event.logical_key, Key::Named(NamedKey::Tab)) {
                            if shift {
                                s.active = s.active.checked_sub(1).unwrap_or(s.tabs.len() - 1);
                            } else {
                                s.active = (s.active + 1) % s.tabs.len();
                            }
                            true
                        }
                        // ── Find / Replace ────────────────────────────────────────────────────
                        else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "f") {
                            s.find.open         = true;
                            s.find.replace_open = false;
                            s.find.focus        = FindFocus::Query;
                            s.find.query.clear();
                            if let Some(t) = s.tab().sel_text() { s.find.query = t; }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "h") {
                            s.find.open         = true;
                            s.find.replace_open = true;
                            s.find.focus        = FindFocus::Query;
                            s.find.query.clear();
                            if let Some(t) = s.tab().sel_text() { s.find.query = t; }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "g") {
                            if !s.find.query.is_empty() { find_step(s, shift); }
                            true
                        }
                        // ── Multi-cursor ──────────────────────────────────────────────────────
                        else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "d") {
                            // If no selection on primary, select word under cursor first
                            if !s.tab().primary().has_sel() {
                                let pos = s.tab().primary().head;
                                let (lo, hi) = word_bounds_at(s.tab(), pos);
                                if lo < hi {
                                    *s.tab_mut().primary_mut() = Cursor { head: hi, tail: lo };
                                }
                            }
                            let query = s.tab().sel_text().unwrap_or_default();
                            if !query.is_empty() {
                                let ms = find_matches(&s.tab().text, &query,
                                                      s.find.case_sensitive, s.find.whole_word);
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
                        } else if cmd && shift && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "l") {
                            if let Some(query) = s.tab().sel_text().filter(|t| !t.is_empty()) {
                                let ms = find_matches(&s.tab().text, &query,
                                                      s.find.case_sensitive, s.find.whole_word);
                                if !ms.is_empty() {
                                    s.tab_mut().cursors = ms.into_iter()
                                        .map(|(lo, hi)| Cursor { head: hi, tail: lo })
                                        .collect();
                                    s.dedup_cursors();
                                }
                            }
                            true
                        }
                        // ── Escape: collapse multi-cursor ─────────────────────────────────────
                        else if matches!(&event.logical_key, Key::Named(NamedKey::Escape)) {
                            if s.tab().cursors.len() > 1 {
                                let head = s.tab().primary().head;
                                s.tab_mut().cursors = vec![Cursor::new(head)];
                                true
                            } else { false }
                        } else if ctrl && matches!(&event.logical_key, Key::Character(_)) {
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
                                    if cmd { s.move_doc_start(shift); } else { s.move_up(shift); }
                                }
                                Key::Named(NamedKey::ArrowDown) => {
                                    if cmd { s.move_doc_end(shift); } else { s.move_down(shift); }
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

                    render(s);
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
        if let Some(s) = &self.state {
            el.set_control_flow(ControlFlow::WaitUntil(s.cursor_blink));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
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

fn render(s: &mut State) {
    let w = s.w;
    let h = s.h;
    if w == 0 || h == 0 { return; }
    dlog!("[render] {}x{} t={}", w, h, ts());

    let explorer_w   = s.explorer_w();
    let scroll       = s.tab().scroll;
    let hscroll      = s.tab().hscroll;
    let tab_h        = s.tab_h();
    let status_h     = s.status_h();
    let find_h       = s.find_h();
    let editor_h     = s.editor_h();
    let lh           = s.glyphs.lh;
    let asc          = s.glyphs.asc;
    let cw           = s.glyphs.cw;
    let total        = s.tab().text.len_lines();
    let vis          = (editor_h / lh).max(1) as usize;
    let cursor_visible = s.cursor_visible;

    // Cursors snapshot: (line, col, sel)
    let cursors_snap: Vec<(usize, usize, Option<(usize, usize)>)> = {
        let t = &s.tabs[s.active];
        t.cursors.iter().map(|c| {
            let head = c.head.min(t.text.len_chars());
            let line = t.text.char_to_line(head);
            let col  = head - t.text.line_to_char(line);
            (line, col, c.sel())
        }).collect()
    };
    let (cur_line, cur_col) = cursors_snap.last().map(|&(l, c, _)| (l, c)).unwrap_or((0, 0));

    // Find bar snapshot
    let find_open      = s.find.open;
    let find_repl_open = s.find.replace_open;
    let find_focus     = s.find.focus;
    let case_sensitive = s.find.case_sensitive;
    let whole_word     = s.find.whole_word;
    let find_query     = s.find.query.clone();
    let find_repl      = s.find.replace.clone();
    let find_matches_snap: Vec<(usize, usize)> = if find_open && !find_query.is_empty() {
        find_matches(&s.tab().text, &find_query, case_sensitive, whole_word)
    } else { vec![] };

    // Language / syntax
    let lang = s.tab().path.as_deref().map(Lang::from_path).unwrap_or(Lang::None);
    let mut hl_state = MlState::Normal;
    if lang != Lang::None {
        for li in 0..scroll {
            let chars: Vec<char> = s.tab().text.line(li)
                .chars().take_while(|&c| c != '\n' && c != '\r').collect();
            let (_, ns) = highlight_line(&chars, lang, hl_state);
            hl_state = ns;
        }
    }

    let line_count = vis.min(total.saturating_sub(scroll));
    let mut lines: Vec<(String, usize, Vec<u32>)> = Vec::with_capacity(line_count);
    for vi in 0..line_count {
        let li         = scroll + vi;
        let line_start = s.tab().text.line_to_char(li);
        let chars: Vec<char> = s.tab().text.line(li)
            .chars().take_while(|&c| c != '\n' && c != '\r').collect();
        let text: String = chars.iter().collect();
        let (colors, ns) = if lang != Lang::None {
            highlight_line(&chars, lang, hl_state)
        } else {
            (vec![FG; chars.len()], hl_state)
        };
        hl_state = ns;
        lines.push((text, line_start, colors));
    }

    let max_line_len = (0..total).map(|li| State::line_len(&s.tab().text, li)).max().unwrap_or(0);
    let path_name    = s.tab().display_name().to_owned();
    let dirty        = s.tab().dirty;

    let tab_info: Vec<(String, bool)> = s.tabs.iter()
        .map(|t| (t.display_name().to_owned(), t.dirty))
        .collect();
    let active_tab = s.active;

    let show_hidden = s.explorer.as_ref().map_or(false, |ex| ex.show_hidden);
    let explorer_snap: Option<Vec<(String, bool, bool, usize, bool)>> =
        s.explorer.as_ref().map(|ex| {
            ex.entries.iter().enumerate().map(|(i, e)| {
                (e.name.clone(), e.is_dir, e.expanded, e.depth, i == ex.selected)
            }).collect()
        });

    let glyphs = &s.glyphs as *const Glyphs;

    s.renderer.render_frame(move |buf, w, h| {
        let g = unsafe { &*glyphs };

        // ── Clear ─────────────────────────────────────────────────────────
        for p in buf.iter_mut() { *p = BG; }

        // ── Explorer panel ────────────────────────────────────────────────
        if let Some(entries) = &explorer_snap {
            let panel_h = h as i32 - tab_h - status_h;
            fill(buf, w, h, 0, tab_h, explorer_w, panel_h, BG2);
            fill(buf, w, h, explorer_w - 1, tab_h, 1, panel_h, BORDER);

            let toggle_label = if show_hidden { "  [x] .hidden" } else { "  [ ] .hidden" };
            draw_str(buf, w, h, g, toggle_label, 0, tab_h + asc, FG_DIM, explorer_w - 1);
            fill(buf, w, h, 0, tab_h + lh - 1, explorer_w - 1, 1, BORDER);

            for (i, (name, is_dir, expanded, depth, selected)) in entries.iter().enumerate() {
                let ey = tab_h + lh + i as i32 * lh;
                if ey + lh > h as i32 - status_h { break; }
                let baseline = ey + asc;
                if *selected { fill(buf, w, h, 0, ey, explorer_w - 1, lh, SEL_BG); }
                let prefix = if *is_dir { if *expanded { "▼ " } else { "▶ " } } else { "  " };
                let indent = *depth as i32 * 10 + 4;
                let label  = format!("{prefix}{name}");
                draw_str(buf, w, h, g, &label, indent, baseline, FG, explorer_w - 1);
            }
        }

        // ── Tab bar ───────────────────────────────────────────────────────
        fill(buf, w, h, 0, 0, w as i32, tab_h, BG2);
        fill(buf, w, h, 0, tab_h - 1, w as i32, 1, BORDER);

        let mut tx = 0i32;
        for (i, (name, dirty_tab)) in tab_info.iter().enumerate() {
            let label    = if *dirty_tab { format!(" {}• ", name) } else { format!(" {}  ", name) };
            let tw       = label.chars().count() as i32 * cw;
            let is_active = i == active_tab;
            let tab_bg   = if is_active { BG } else { BG2 };
            fill(buf, w, h, tx, 0, tw, tab_h - 1, tab_bg);
            if is_active { fill(buf, w, h, tx, tab_h - 2, tw, 2, ACCENT); }
            draw_str(buf, w, h, g, &label, tx, tab_h * 3 / 4, FG, tx + tw);
            fill(buf, w, h, tx + tw, 0, 1, tab_h, BORDER);
            tx += tw + 1;
        }

        // ── Editor lines ──────────────────────────────────────────────────
        let ed_x = explorer_w + ED_LPAD;
        for (vi, (text, line_start, colors)) in lines.iter().enumerate() {
            let li       = scroll + vi;
            let py       = tab_h + vi as i32 * lh;
            let baseline = py + asc;

            // Find match highlights (drawn first, lowest layer)
            for &(mlo, mhi) in &find_matches_snap {
                let lcc      = text.chars().count();
                let line_end = line_start + lcc;
                if mlo < line_end + 1 && mhi > *line_start {
                    let is_active = cursors_snap.iter().any(|(_, _, sel)| {
                        sel.map_or(false, |(lo, hi)| lo == mlo && hi == mhi)
                    });
                    let color  = if is_active { HL_MATCH_ACTIVE } else { HL_MATCH };
                    let col_lo = mlo.saturating_sub(*line_start);
                    let col_hi = mhi.saturating_sub(*line_start).min(lcc);
                    let sx     = (ed_x + (col_lo as i32 - hscroll as i32) * cw).max(ed_x);
                    let ex     = ed_x + (col_hi as i32 - hscroll as i32) * cw;
                    let sw     = (ex - sx).max(0);
                    if sw > 0 { fill(buf, w, h, sx, py, sw, lh, color); }
                }
            }

            // Selection highlights for all cursors
            for &(_, _, sel) in &cursors_snap {
                if let Some((sel_lo, sel_hi)) = sel {
                    let line_char_count = text.chars().count();
                    let line_end        = line_start + line_char_count;
                    if sel_lo < line_end + 1 && sel_hi > *line_start {
                        let col_lo = sel_lo.saturating_sub(*line_start);
                        let col_hi = if sel_hi > line_end { line_char_count + 1 }
                                     else { sel_hi - line_start };
                        let sx_raw = ed_x + (col_lo as i32 - hscroll as i32) * cw;
                        let sx_end = ed_x + (col_hi as i32 - hscroll as i32) * cw;
                        let sx     = sx_raw.max(ed_x);
                        let sw     = (sx_end - sx).max(0);
                        if sw > 0 { fill(buf, w, h, sx, py, sw, lh, SEL_BG); }
                    }
                }
            }

            // Text
            let mut x = ed_x - hscroll as i32 * cw;
            for (ci, ch) in text.chars().enumerate() {
                if x + cw > 0 && x < w as i32 {
                    let color = colors.get(ci).copied().unwrap_or(FG);
                    if let Some((m, bmap)) = g.get(ch) {
                        blit(buf, w, h, bmap, m, x, baseline, color);
                    }
                }
                x += cw;
                if x >= w as i32 { break; }
            }

            // All cursors on this line
            for &(c_line, c_col, _) in &cursors_snap {
                if c_line == li && cursor_visible {
                    let cx = ed_x + (c_col as i32 - hscroll as i32) * cw;
                    if cx >= ed_x { fill(buf, w, h, cx, py, 2, lh, ACCENT); }
                }
            }
        }

        // ── Scrollbars ────────────────────────────────────────────────────
        let editor_w = w as i32 - explorer_w;

        if total > vis {
            let track_h = editor_h;
            let thumb_h = ((track_h * vis as i32) / total as i32).max(SB_W);
            let thumb_y = tab_h + ((scroll as i32 * (track_h - thumb_h)) / (total - vis) as i32);
            fill(buf, w, h, w as i32 - SB_W, tab_h, SB_W, track_h, BG2);
            fill(buf, w, h, w as i32 - SB_W, thumb_y, SB_W, thumb_h, SB_THUMB);
        }

        let vis_cols = (editor_w / cw).max(1) as usize;
        if max_line_len > vis_cols {
            let track_w = editor_w - if total > vis { SB_W } else { 0 };
            let thumb_w = ((track_w * vis_cols as i32) / max_line_len as i32).max(SB_W);
            let thumb_x = explorer_w + ((hscroll as i32 * (track_w - thumb_w)) / (max_line_len - vis_cols) as i32);
            let sb_y    = h as i32 - status_h - find_h - SB_W;
            fill(buf, w, h, explorer_w, sb_y, track_w, SB_W, BG2);
            fill(buf, w, h, thumb_x, sb_y, thumb_w, SB_W, SB_THUMB);
        }

        // ── Find bar panel ────────────────────────────────────────────────
        if find_open {
            let row_h   = lh + 4;
            let find_y  = h as i32 - status_h - find_h;
            fill(buf, w, h, 0, find_y, w as i32, find_h, BG2);
            fill(buf, w, h, 0, find_y, w as i32, 1, BORDER);

            // Row 1: Find
            let row1_base = find_y + row_h * 3 / 4;
            let label     = "Find: ";
            let lw        = label.len() as i32 * cw;
            draw_str(buf, w, h, g, label, 4, row1_base, FG_DIM, w as i32);

            // Toggles [Aa] [W] on the right
            let aa_str    = if case_sensitive { "[Aa]" } else { "[aa]" };
            let ww_str    = if whole_word     { "[W]"  } else { "[w]"  };
            let toggle_w  = (aa_str.len() + 1 + ww_str.len()) as i32 * cw + 8;
            let aa_x      = w as i32 - toggle_w;
            let ww_x      = aa_x + (aa_str.len() + 1) as i32 * cw;
            draw_str(buf, w, h, g, aa_str, aa_x, row1_base,
                     if case_sensitive { ACCENT } else { FG_DIM }, w as i32);
            draw_str(buf, w, h, g, ww_str, ww_x, row1_base,
                     if whole_word     { ACCENT } else { FG_DIM }, w as i32);

            // Match count
            let mc_str = format!("{} matches", find_matches_snap.len());
            let mc_w   = mc_str.len() as i32 * cw;
            let mc_x   = aa_x - mc_w - cw;
            draw_str(buf, w, h, g, &mc_str, mc_x, row1_base, FG_DIM, mc_x + mc_w + cw);

            // Query text + cursor
            let qx    = 4 + lw;
            let qclip = mc_x - cw;
            draw_str(buf, w, h, g, &find_query, qx, row1_base, FG, qclip);
            if find_focus == FindFocus::Query {
                let cx = qx + find_query.chars().count() as i32 * cw;
                if cx < qclip { fill(buf, w, h, cx, find_y + 2, 2, lh, ACCENT); }
            }

            // Row 2: Replace
            if find_repl_open {
                let row2_y    = find_y + row_h;
                let row2_base = row2_y + row_h * 3 / 4;
                fill(buf, w, h, 0, row2_y, w as i32, 1, BORDER);

                let rlabel = "Replace: ";
                let rlw    = rlabel.len() as i32 * cw;
                draw_str(buf, w, h, g, rlabel, 4, row2_base, FG_DIM, w as i32);

                // Buttons [Repl] [All]
                let repl_str = "[Repl]";
                let all_str  = "[All]";
                let btn_w    = (repl_str.len() + 1 + all_str.len()) as i32 * cw + 8;
                let btn_x    = w as i32 - btn_w;
                let all_x    = btn_x + (repl_str.len() + 1) as i32 * cw;
                draw_str(buf, w, h, g, repl_str, btn_x, row2_base, FG_DIM, w as i32);
                draw_str(buf, w, h, g, all_str,  all_x, row2_base, FG_DIM, w as i32);

                let rx    = 4 + rlw;
                let rclip = btn_x - cw;
                draw_str(buf, w, h, g, &find_repl, rx, row2_base, FG, rclip);
                if find_focus == FindFocus::Replace {
                    let cx = rx + find_repl.chars().count() as i32 * cw;
                    if cx < rclip { fill(buf, w, h, cx, row2_y + 2, 2, lh, ACCENT); }
                }
            }
        }

        // ── Status bar ────────────────────────────────────────────────────
        let sy = h as i32 - status_h;
        fill(buf, w, h, 0, sy, w as i32, status_h, BG2);
        fill(buf, w, h, 0, sy, w as i32, 1, BORDER);

        let sbase      = sy + status_h * 3 / 4;
        let dirty_mark = if dirty { " *" } else { "" };
        let name_str   = format!("  {path_name}{dirty_mark}");
        draw_str(buf, w, h, g, &name_str, 0, sbase, FG, w as i32);

        let lc_str = format!("Ln {}, Col {}  ", cur_line + 1, cur_col + 1);
        let lc_w   = lc_str.chars().count() as i32 * cw;
        draw_str(buf, w, h, g, &lc_str, w as i32 - lc_w, sbase, FG_DIM, w as i32);
    });
}

// ── Entry point ───────────────────────────────────────────────────────────────
fn main() {
    let arg = std::env::args().nth(1).map(PathBuf::from);
    let (file_arg, dir_arg) = match arg {
        Some(p) if p.is_dir() => (None, Some(p)),
        Some(p)               => (Some(p), None),
        None                  => (None, None),
    };
    let el = EventLoop::new().unwrap();
    let mut app = App::new(file_arg, dir_arg);
    el.run_app(&mut app).unwrap();
}
