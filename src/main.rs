mod platform;
mod settings;
mod terminal;
mod lsp;

use std::collections::HashMap;
use std::path::PathBuf;
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
use winit::keyboard::{Key, ModifiersState, NamedKey};
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

// ── Language detection ────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Debug)]
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
const FONT_PX:  f32 = 14.0;
const ED_LPAD:  i32 = 6;
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
        for ch in ['▶', '▼', '•', '×'] { self.load(ch); }
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
#[derive(Clone, PartialEq)]
enum TabKind { Editor, Settings }

#[derive(Clone)]
struct Tab {
    kind:         TabKind,
    buf_id:       usize,   // shared with sibling tabs showing the same buffer
    text:         Rope,
    path:         Option<PathBuf>,
    dirty:        bool,
    cursors:      Vec<Cursor>, // always non-empty; primary = last element
    scroll:       usize,
    hscroll:      usize,
    undo_stack:   Vec<UndoEntry>,
    redo_stack:   Vec<UndoEntry>,
    last_typing:  bool, // for coalescing consecutive single-char inserts
}

impl Tab {
    fn untitled(buf_id: usize) -> Self {
        Tab { kind: TabKind::Editor, buf_id, text: Rope::new(), path: None, dirty: false,
              cursors: vec![Cursor::new(0)], scroll: 0, hscroll: 0,
              undo_stack: Vec::new(), redo_stack: Vec::new(), last_typing: false }
    }

    fn settings() -> Self {
        Tab { kind: TabKind::Settings, buf_id: usize::MAX, text: Rope::new(),
              path: None, dirty: false, cursors: vec![Cursor::new(0)],
              scroll: 0, hscroll: 0,
              undo_stack: Vec::new(), redo_stack: Vec::new(), last_typing: false }
    }

    fn display_name(&self) -> &str {
        match self.kind {
            TabKind::Settings => "Settings",
            TabKind::Editor   => self.path.as_deref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("Untitled"),
        }
    }

    fn is_empty_untitled(&self) -> bool {
        self.kind == TabKind::Editor && self.path.is_none() && self.text.len_chars() == 0
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
                self.text       = Rope::from_str(&content);
                self.path       = Some(path);
                self.cursors    = vec![Cursor::new(0)];
                self.scroll     = 0;
                self.hscroll    = 0;
                self.dirty      = false;
                self.undo_stack = Vec::new();
                self.redo_stack = Vec::new();
                self.last_typing = false;
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
enum PaneKind { Editor, Terminal, LspOutput }

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
                s.panes.remove(&src_pid);
                let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
                if let Some(t) = remove_pane_from_tree(old_tree, src_pid) { s.pane_tree = t; }
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

fn open_terminal_pane(s: &mut State) {
    let pane_id = s.next_pane_id; s.next_pane_id += 1;
    let term_id = s.next_pane_id; s.next_pane_id += 1;
    let area = s.pane_area();
    let cols = (area.w / s.glyphs.cw).max(1) as usize;
    let rows = ((area.h / 2) / s.glyphs.lh).max(1) as usize;
    let proxy = s.proxy.clone();
    let tp = terminal::spawn_terminal(term_id, cols, rows, proxy);
    s.term_panes.insert(term_id, tp);
    let pane = Pane { id: pane_id, kind: PaneKind::Terminal, tabs: vec![],
                      term_ids: vec![term_id], active: 0, find: FindBar::new() };
    s.panes.insert(pane_id, pane);
    let active = s.active_pane;
    let old_tree = std::mem::replace(&mut s.pane_tree, PaneTree::Leaf(0));
    s.pane_tree = insert_pane(old_tree, active, pane_id, DropZone::Bottom);
    s.active_pane = pane_id;
}

fn open_settings_tab(s: &mut State) {
    if s.panes.get(&s.active_pane).map_or(false, |p| p.kind != PaneKind::Editor) { return; }
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

pub enum UserEvent {
    TermOutput     { pane_id: usize, data: Vec<u8> },
    LspOutput      { pane_id: usize, data: Vec<u8> },
    LspDiagnostics { path: PathBuf, diagnostics: Vec<Diagnostic> },
    Redraw,
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

    term_panes:  HashMap<usize, terminal::TermPane>,
    lsp_panes:   HashMap<usize, OutputPane>,
    lsp:         lsp::LspManager,
    diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    proxy:       EventLoopProxy<UserEvent>,

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
    last_click_time: Instant,
    last_click_char: usize,
    click_count:     u32,

    settings:     settings::Settings,
    needs_redraw: bool,
}

impl State {
    fn pane(&self)         -> &Pane     { &self.panes[&self.active_pane] }
    fn pane_mut(&mut self) -> &mut Pane { let id = self.active_pane; self.panes.get_mut(&id).unwrap() }
    fn tab(&self)          -> &Tab      { self.pane().tab() }
    fn tab_mut(&mut self)  -> &mut Tab  { let id = self.active_pane; self.panes.get_mut(&id).unwrap().tab_mut() }
    fn find(&self)         -> &FindBar  { &self.pane().find }
    fn find_mut(&mut self) -> &mut FindBar { let id = self.active_pane; &mut self.panes.get_mut(&id).unwrap().find }

    fn explorer_w(&self) -> i32 {
        if self.explorer.is_some() { self.explorer_w } else { 0 }
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
    fn find_h(&self) -> i32 { Self::pane_find_h(self.pane(), self.glyphs.lh) }

    fn pane_area(&self) -> Rect {
        let ew = self.explorer_w();
        Rect { x: ew, y: 0, w: self.w as i32 - ew, h: self.h as i32 - self.status_h() }
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
        if line >= t.scroll  + vis_v   { t.scroll  = line + 1 - vis_v; }
        if col  < t.hscroll             { t.hscroll = col; }
        if col  >= t.hscroll + vis_h   { t.hscroll = col + 1 - vis_h; }
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
        let tab = self.tab_mut();
        if !coalesce || !tab.last_typing {
            if tab.undo_stack.len() >= 1000 { tab.undo_stack.remove(0); }
            tab.undo_stack.push(UndoEntry { text: tab.text.clone(), cursors: tab.cursors.clone() });
            tab.redo_stack.clear();
        }
        tab.last_typing = coalesce;
    }

    fn undo(&mut self) {
        let Some(entry) = self.tab_mut().undo_stack.pop() else { return };
        let cur = UndoEntry { text: self.tab().text.clone(), cursors: self.tab().cursors.clone() };
        let tab = self.tab_mut();
        tab.redo_stack.push(cur);
        tab.text    = entry.text;
        tab.cursors = entry.cursors;
        tab.dirty   = true;
        tab.last_typing = false;
        self.ensure_visible();
    }

    fn redo(&mut self) {
        let Some(entry) = self.tab_mut().redo_stack.pop() else { return };
        let cur = UndoEntry { text: self.tab().text.clone(), cursors: self.tab().cursors.clone() };
        let tab = self.tab_mut();
        tab.undo_stack.push(cur);
        tab.text    = entry.text;
        tab.cursors = entry.cursors;
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
        let last = { let t = self.tab(); Self::last_line(&t.text) };
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
fn open_or_reuse_tab(s: &mut State, path: PathBuf) {
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
        if pane.tabs[i].path.as_deref() == Some(path.as_path()) {
            pane.active = i;
            return;
        }
    }
    if pane.tab().is_empty_untitled() {
        pane.tab_mut().load_file(path.clone());
    } else {
        let mut tab = Tab::untitled(s.next_buf_id);
        s.next_buf_id += 1;
        tab.load_file(path.clone());
        pane.tabs.push(tab);
        pane.active = pane.tabs.len() - 1;
    }
    // Notify LSP of the opened file
    notify_lsp_open(s, &path);
}

fn notify_lsp_open(s: &mut State, path: &PathBuf) {
    let lang = Lang::from_path(path);
    if lang == Lang::None { return; }
    // Start server if not running
    if !s.lsp.has_server_for(lang) {
        let op_id = s.next_pane_id;
        s.next_pane_id += 1;
        let proxy = s.proxy.clone();
        if let Some(mut srv) = lsp::start_server(lang, op_id, proxy) {
            // Send initialize request
            let root = path.parent().map(|p| p.to_path_buf());
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
        let lang = Lang::from_path(&path);
        if lang == Lang::None { return; }
        (path, tab.text.to_string(), lang)
    };
    if let Some(srv) = s.lsp.server_for_lang_mut(lang) {
        lsp::notify_did_change(srv, &path, &text);
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

// ── App ───────────────────────────────────────────────────────────────────────
struct App {
    state:        Option<State>,
    file_arg:     Option<PathBuf>,
    dir_arg:      Option<PathBuf>,
    proxy:        EventLoopProxy<UserEvent>,
    dirty:        Arc<AtomicBool>,
    display_link: Option<platform::DisplayLink>,
}

impl App {
    fn new(file_arg: Option<PathBuf>, dir_arg: Option<PathBuf>, proxy: EventLoopProxy<UserEvent>) -> Self {
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
            if let Some(s) = self.state.as_mut() {
                dlog!("[blink] t={}", ts());
                s.cursor_visible = !s.cursor_visible;
                s.cursor_blink = Instant::now() + Duration::from_millis(500);
                // Mark dirty; vsync display link or about_to_wait flush will trigger the render.
                self.dirty.store(true, Ordering::Release);
                s.needs_redraw = true;
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
            settings::RendererBackend::Gpu => platform::Renderer::new_gpu(&win),
            settings::RendererBackend::Cpu => platform::Renderer::new_cpu(&win),
        };

        let font_size = loaded_settings.font_size;
        let glyphs = Glyphs::new(include_bytes!("../assets/JetBrainsMono-Regular.ttf"), font_size);

        let mut initial_pane = Pane::new(0, 0); // pane 0, buf 0
        if let Some(path) = self.file_arg.take() {
            initial_pane.tabs[0].load_file(path);
        }
        let mut panes = HashMap::new();
        panes.insert(0usize, initial_pane);

        let explorer = self.dir_arg.take().map(FileExplorer::new);
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
            lsp:         lsp::LspManager::new(),
            diagnostics: HashMap::new(),
            proxy:       self.proxy.clone(),
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
            last_click_time: Instant::now() - Duration::from_secs(1),
            last_click_char: usize::MAX,
            click_count:     0,

            settings:     loaded_settings,
            needs_redraw: false,
        };

        s.win.request_redraw();
        self.state = Some(s);
        self.apply_vsync_setting();
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
                resize_terminal_panes(s);
                { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
            }

            WindowEvent::ModifiersChanged(m) => {
                s.mods = m.state();
            }

            WindowEvent::CursorMoved { position, .. } => {
                s.mouse_x = position.x as f32;
                s.mouse_y = position.y as f32;
                let mx = s.mouse_x as i32;
                let my = s.mouse_y as i32;

                // Explorer border drag
                if s.explorer_drag {
                    s.explorer_w = mx.clamp(80, 600);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
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
                    s.win.set_cursor(if in_ed { CursorIcon::Text } else { CursorIcon::Default });
                }
                if s.mouse_down {
                    let pos = s.xy_to_char(mx, my);
                    s.tab_mut().primary_mut().head = pos;
                    s.ensure_visible();
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                }
                // Terminal mouse motion reporting
                {
                    let pane_id = s.active_pane;
                    if s.panes.get(&pane_id).map_or(false, |p| p.kind == PaneKind::Terminal) {
                        let tid = s.panes[&pane_id].term_ids.get(s.panes[&pane_id].active).copied();
                        if let Some(tid) = tid {
                            if let Some(tp) = s.term_panes.get(&tid) {
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
                                    unsafe { libc::write(pty_fd, bytes.as_ptr().cast(), bytes.len()); }
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
                                    unsafe { libc::write(pty_fd, bytes.as_ptr().cast(), bytes.len()); }
                                }
                            }
                        }
                    }
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left, ..
            } => {
                let mx  = s.mouse_x as i32;
                let my  = s.mouse_y as i32;
                let alt = s.mods.alt_key();

                // Explorer border drag start (before pane border and explorer click)
                if s.explorer.is_some() {
                    let bx = s.explorer_w();
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

                // Explorer panel click
                if s.explorer.is_some() && mx < s.explorer_w() {
                    let lh  = s.glyphs.lh;
                    let row = my / lh;
                    if row == 0 {
                        if let Some(ex) = s.explorer.as_mut() { ex.toggle_hidden(); }
                    } else if row > 0 {
                        let idx = (row - 1) as usize;
                        let action = s.explorer.as_mut().and_then(|ex| {
                            if idx < ex.entries.len() {
                                ex.selected = idx;
                                if ex.entries[idx].is_dir { ex.toggle(idx); None }
                                else { Some(ex.entries[idx].path.clone()) }
                            } else { None }
                        });
                        if let Some(path) = action { open_or_reuse_tab(s, path); }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Status bar — ignore
                if my >= s.h as i32 - s.status_h() { return; }

                // Which pane was clicked?
                let area = s.pane_area();
                let Some(clicked_pane_id) = pane_at_pos(&s.pane_tree, mx, my, area) else { return };
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
                        let tid = s.panes[&clicked_pane_id].term_ids
                            .get(s.panes[&clicked_pane_id].active).copied();
                        if let Some(tid) = tid {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                if tp.grid.mouse_report != terminal::MouseReportMode::None {
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
                                    let cb = mod_bits; // left button = 0
                                    let sgr = tp.grid.mouse_sgr;
                                    let pty_fd = tp.pty_fd;
                                    let bytes = terminal::encode_mouse(term_col, term_row, cb, true, sgr);
                                    unsafe { libc::write(pty_fd, bytes.as_ptr().cast(), bytes.len()); }
                                    s.term_buttons_held |= 1;
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
                    let btn_x     = pane_rect.x + 14 * cw;
                    let ry        = content_y + lh + 8;
                    let vy        = ry + lh + 4;
                    if my >= ry && my < ry + lh {
                        if mx >= btn_x && mx < btn_x + 5 * cw && s.renderer.is_gpu() {
                            s.settings.renderer = settings::RendererBackend::Cpu;
                            s.renderer = platform::Renderer::new_cpu(&s.win);
                            s.renderer.resize(s.w, s.h);
                            s.settings.save();
                        } else if mx >= btn_x + 6 * cw && mx < btn_x + 11 * cw && !s.renderer.is_gpu() {
                            s.settings.renderer = settings::RendererBackend::Gpu;
                            s.renderer = platform::Renderer::new_gpu(&s.win);
                            s.renderer.resize(s.w, s.h);
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
                        else                       { s.find_mut().focus = FindFocus::Query; }
                    } else if rel_row == 1 && s.find().replace_open {
                        s.find_mut().focus = FindFocus::Replace;
                        let repl_len = 6usize;
                        let all_len  = 5usize;
                        let btn_w    = (repl_len + 1 + all_len) as i32 * cw + 8;
                        let btn_x    = s.w as i32 - btn_w;
                        let all_x    = btn_x + (repl_len + 1) as i32 * cw;
                        if mx >= btn_x && mx < all_x { replace_current(s); }
                        else if mx >= all_x          { replace_all(s); }
                    }
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Editor area click
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
                        let cw = s.glyphs.cw;
                        let lh = s.glyphs.lh;
                        (-(p.x as i32) / cw, -(p.y as i32) / lh)
                    }
                };
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
                                    unsafe { libc::write(pty_fd, bytes.as_ptr().cast(), bytes.len()); }
                                }
                            } else {
                                let tp = s.term_panes.get_mut(&tid).unwrap();
                                let sb = tp.grid.scrollback.len();
                                if dy < 0 {
                                    tp.grid.scroll_offset = (tp.grid.scroll_offset + (-dy) as usize).min(sb);
                                } else {
                                    tp.grid.scroll_offset = tp.grid.scroll_offset.saturating_sub(dy as usize);
                                }
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
                    PaneKind::Editor => {
                        if dy != 0 { s.scroll_by(dy); }
                        if dx != 0 { s.hscroll_by(dx); }
                        if dx != 0 || dy != 0 { { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); } }
                    }
                }
            }

            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                dlog!("[input] {:?}", event.logical_key);
                let ctrl  = s.mods.control_key();
                let cmd   = s.mods.super_key();
                let alt   = s.mods.alt_key();
                let shift = s.mods.shift_key();

                // Cmd+, — open/focus Settings tab
                if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == ",") {
                    open_settings_tab(s);
                    { s.needs_redraw = true; self.dirty.store(true, Ordering::Release); }
                    return;
                }

                // Ctrl+` — open a new terminal pane (works from any pane kind)
                if ctrl && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "`") {
                    open_terminal_pane(s);
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
                        let shell = {
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
                    // Forward all other key events to the active PTY
                    let bytes = terminal::encode_key(&event.logical_key, s.mods, event.text.as_deref());
                    if let Some(bytes) = bytes {
                        let p = &s.panes[&s.active_pane];
                        if let Some(&tid) = p.term_ids.get(p.active) {
                            if let Some(tp) = s.term_panes.get(&tid) {
                                unsafe { libc::write(tp.pty_fd, bytes.as_ptr().cast(), bytes.len()); }
                            }
                        }
                    }
                    return;
                }

                // LspOutput pane: read-only, ignore all input except pane-switch shortcuts
                if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::LspOutput) {
                    return;
                }

                // Find bar: route non-cmd keys when open
                if s.find().open && !cmd {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => { s.find_mut().open = false; }
                        Key::Named(NamedKey::Tab) => {
                            if s.find().replace_open {
                                let nf = if s.find().focus == FindFocus::Query { FindFocus::Replace } else { FindFocus::Query };
                                s.find_mut().focus = nf;
                            }
                        }
                        Key::Named(NamedKey::Enter) => {
                            if s.find().focus == FindFocus::Replace && s.find().replace_open {
                                replace_current(s);
                            } else {
                                find_step(s, shift);
                            }
                        }
                        Key::Named(NamedKey::Backspace) => { s.find_mut().active_field_mut().pop(); }
                        _ => {
                            if let Some(txt) = event.text.as_deref() {
                                s.find_mut().active_field_mut().push_str(txt);
                            }
                        }
                    }
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
                            if let Some(text) = s.tab().sel_text() { clipboard_set(&text); }
                            true
                        } else if cmd && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "x") {
                            if let Some(text) = s.tab().sel_text() {
                                clipboard_set(&text);
                                s.push_undo(false);
                                let order = s.cursor_order_ltr();
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
                                pane.tabs.remove(pane.active);
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
                            } else if s.explorer.is_some() {
                                // Directory open: reset to empty tab rather than exiting
                                let new_buf_id = s.next_buf_id; s.next_buf_id += 1;
                                let active = s.pane().active;
                                s.pane_mut().tabs[active] = Tab::untitled(new_buf_id);
                            } else {
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
                        } else if ctrl && matches!(&event.logical_key, Key::Character(_)) {
                            false
                        } else if active_tab_is_settings {
                            false // settings tab: block all text editing
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
            el.set_control_flow(ControlFlow::WaitUntil(s.cursor_blink));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, event: UserEvent) {
        let Some(s) = self.state.as_mut() else { return };
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
            UserEvent::Redraw => {}
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
    // Diagnostics: (line, col_start, col_end, severity)
    diagnostics:    Vec<(usize, usize, usize, DiagSeverity)>,
}

fn render(s: &mut State) {
    let w = s.w;
    let h = s.h;
    if w == 0 || h == 0 { return; }
    dlog!("[render] {}x{} t={}", w, h, ts());

    // Sync shared buffers: propagate the active tab's text/path/dirty to all
    // other tabs with the same buf_id (O(1) Rope clone per sibling).
    // Only applies to editor panes (terminal/output panes have no tabs).
    if s.panes.get(&s.active_pane).map_or(false, |p| p.kind == PaneKind::Editor) {
        let ap = s.active_pane;
        let at = s.panes[&ap].active;
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
    }
    let term_snaps: Vec<TermPaneSnap> = layout.iter().filter_map(|&(pid, rect)| {
        let pane = s.panes.get(&pid)?;
        if pane.kind != PaneKind::Terminal { return None; }
        let active_tid = pane.term_ids.get(pane.active).copied()?;
        let tp = s.term_panes.get(&active_tid)?;
        let tabs: Vec<String> = pane.term_ids.iter()
            .filter_map(|&tid| s.term_panes.get(&tid).map(|t| t.title.clone()))
            .collect();
        Some(TermPaneSnap {
            id: pid,
            rect,
            is_active: pid == active_pane_id,
            visible_rows: tp.grid.visible_rows(),
            cursor_col: tp.grid.cur_col,
            cursor_row: tp.grid.cur_row,
            cursor_visible,
            tabs,
            active_tab: pane.active,
        })
    }).collect();

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

    // Build per-pane snapshots (editor panes only)
    let pane_snaps: Vec<PaneSnap> = layout.iter().filter_map(|&(pid, rect)| {
        let pane = s.panes.get(&pid)?;
        if pane.kind != PaneKind::Editor { return None; }

        // Settings tab: build a minimal snap (no lines/cursors needed)
        if pane.tabs.get(pane.active).map_or(false, |t| t.kind == TabKind::Settings) {
            let tab_info: Vec<(String, bool)> = pane.tabs.iter()
                .map(|t| (t.display_name().to_owned(), t.dirty)).collect();
            return Some(PaneSnap {
                id: pid, rect, is_active: pid == active_pane_id,
                is_settings_tab: true,
                tab_info, active_tab: pane.active,
                scroll: 0, hscroll: 0, find_h: 0, editor_h: 0,
                cursors_snap: vec![], match_ranges: vec![],
                find_open: false, find_repl_open: false,
                find_focus: FindFocus::Query, case_sensitive: false, whole_word: false,
                find_query: String::new(), find_repl: String::new(),
                lines: vec![], total: 0, max_line_len: 0,
                gutter_w: 0, ln_digits: 0,
                path_name: String::from("Settings"), dirty: false,
                cur_line: 0, cur_col: 0, diagnostics: vec![],
            });
        }

        let tab  = pane.tab();
        let fh   = State::pane_find_h(pane, lh);
        let eh   = (rect.h - tab_h - fh).max(0);
        let vis  = (eh / lh).max(1) as usize;
        let scroll  = tab.scroll;
        let hscroll = tab.hscroll;
        let total   = tab.text.len_lines();
        let lang    = tab.path.as_deref().map(Lang::from_path).unwrap_or(Lang::None);

        let cursors_snap: Vec<(usize, usize, Option<(usize, usize)>)> = tab.cursors.iter().map(|c| {
            let head = c.head.min(tab.text.len_chars());
            let line = tab.text.char_to_line(head);
            let col  = head - tab.text.line_to_char(line);
            (line, col, c.sel())
        }).collect();
        let (cur_line, cur_col) = cursors_snap.last().map(|&(l, c, _)| (l, c)).unwrap_or((0, 0));

        let fq = pane.find.query.clone();
        let match_ranges: Vec<(usize, usize)> = if pane.find.open && !fq.is_empty() {
            find_matches(&tab.text, &fq, pane.find.case_sensitive, pane.find.whole_word)
        } else { vec![] };

        // Syntax highlight lines
        let mut hl_state = MlState::Normal;
        if lang != Lang::None {
            for li in 0..scroll {
                let chars: Vec<char> = tab.text.line(li)
                    .chars().take_while(|&c| c != '\n' && c != '\r').collect();
                let (_, ns) = highlight_line(&chars, lang, hl_state);
                hl_state = ns;
            }
        }
        let line_count = vis.min(total.saturating_sub(scroll));
        let mut lines: Vec<(String, usize, Vec<u32>)> = Vec::with_capacity(line_count);
        for vi in 0..line_count {
            let li         = scroll + vi;
            let line_start = tab.text.line_to_char(li);
            let chars: Vec<char> = tab.text.line(li)
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

        let max_line_len = (0..total).map(|li| State::line_len(&tab.text, li)).max().unwrap_or(0);
        let ln_digits = State::line_num_digits(total);
        let gutter_w  = State::gutter_w(total, cw);
        let tab_info: Vec<(String, bool)> = pane.tabs.iter()
            .map(|t| (t.display_name().to_owned(), t.dirty)).collect();

        Some(PaneSnap {
            id: pid,
            rect,
            is_active: pid == active_pane_id,
            is_settings_tab: false,
            scroll, hscroll, find_h: fh, editor_h: eh,
            cursors_snap, match_ranges,
            find_open: pane.find.open, find_repl_open: pane.find.replace_open,
            find_focus: pane.find.focus, case_sensitive: pane.find.case_sensitive,
            whole_word: pane.find.whole_word,
            find_query: fq, find_repl: pane.find.replace.clone(),
            lines, total, max_line_len, gutter_w, ln_digits,
            tab_info, active_tab: pane.active,
            path_name: tab.display_name().to_owned(), dirty: tab.dirty,
            cur_line, cur_col,
            diagnostics: tab.path.as_ref()
                .and_then(|p| s.diagnostics.get(p))
                .map(|diags| diags.iter().map(|d| (d.line, d.col_start, d.col_end, d.severity.clone())).collect())
                .unwrap_or_default(),
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

    let renderer_is_gpu = s.renderer.is_gpu();
    let vsync_on        = s.settings.vsync;
    let explorer_drag   = s.explorer_drag;
    let ui_scale        = s.font_size / FONT_PX;

    let glyphs = &s.glyphs as *const Glyphs;

    s.renderer.render_frame(move |buf, w, h| {
        let g = unsafe { &*glyphs };

        for p in buf.iter_mut() { *p = BG; }

        // ── Explorer panel ────────────────────────────────────────────────
        if let Some(entries) = &explorer_snap {
            let panel_h = h as i32 - status_h;
            fill(buf, w, h, 0, 0, explorer_w, panel_h, BG2);
            let border_col = if explorer_drag { ACCENT } else { BORDER };
            fill(buf, w, h, explorer_w - 1, 0, 1, panel_h, border_col);

            let toggle_label = if show_hidden { "  [x] .hidden" } else { "  [ ] .hidden" };
            draw_str(buf, w, h, g, toggle_label, 0, asc, FG_DIM, explorer_w - 1);
            fill(buf, w, h, 0, lh - 1, explorer_w - 1, 1, BORDER);

            for (i, (name, is_dir, expanded, depth, selected)) in entries.iter().enumerate() {
                let ey = lh + i as i32 * lh;
                if ey + lh > h as i32 - status_h { break; }
                let baseline = ey + asc;
                if *selected { fill(buf, w, h, 0, ey, explorer_w - 1, lh, SEL_BG); }
                let prefix = if *is_dir { if *expanded { "▼ " } else { "▶ " } } else { "  " };
                let indent = *depth as i32 * 10 + 4;
                let label  = format!("{prefix}{name}");
                draw_str(buf, w, h, g, &label, indent, baseline, FG, explorer_w - 1);
            }
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
                let btn_x     = r.x + 14 * cw;
                // Title separator
                draw_str(buf, w, h, g, "  Settings", r.x, content_y + asc, FG, r.x + r.w);
                fill(buf, w, h, r.x, content_y + lh, r.w, 1, BORDER);
                // Renderer row
                let ry = content_y + lh + 8;
                draw_str(buf, w, h, g, "  Renderer", r.x, ry + asc, FG, btn_x - cw);
                let (cpu_bg, cpu_fg) = if !renderer_is_gpu { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                fill(buf, w, h, btn_x,          ry, 5 * cw, lh, cpu_bg);
                draw_str(buf, w, h, g, " CPU ", btn_x,          ry + asc, cpu_fg, btn_x + 5 * cw);
                let (gpu_bg, gpu_fg) = if  renderer_is_gpu { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                fill(buf, w, h, btn_x + 6 * cw, ry, 5 * cw, lh, gpu_bg);
                draw_str(buf, w, h, g, " GPU ", btn_x + 6 * cw, ry + asc, gpu_fg, btn_x + 11 * cw);
                // VSync row
                let vy = ry + lh + 4;
                draw_str(buf, w, h, g, "  VSync", r.x, vy + asc, FG, btn_x - cw);
                let (vs_bg, vs_fg) = if vsync_on { (ACCENT, BG) } else { (SEL_BG, FG_DIM) };
                let vs_label = if vsync_on { " [x] On " } else { " [ ] Off" };
                fill(buf, w, h, btn_x, vy, 8 * cw, lh, vs_bg);
                draw_str(buf, w, h, g, vs_label, btn_x, vy + asc, vs_fg, btn_x + 8 * cw);
                // UI Scale row
                let sy = vy + lh + 4;
                draw_str(buf, w, h, g, "  UI Scale", r.x, sy + asc, FG, btn_x - cw);
                let scale_str = format!("  {:.0}%  (Cmd+= / Cmd+-)", ui_scale * 100.0);
                draw_str(buf, w, h, g, &scale_str, btn_x, sy + asc, FG_DIM, r.x + r.w);
                // Info row
                let info = if renderer_is_gpu {
                    "  GPU (+~66 MB at 4K) — no tearing at any size"
                } else {
                    "  CPU (no extra RAM) — coalesced + vsync-aligned"
                };
                draw_str(buf, w, h, g, info, r.x, sy + lh + 4 + asc, FG_DIM, r.x + r.w);
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
                    let has_error = snap.diagnostics.iter().any(|&(dl, _, _, ref s)| dl == li && *s == DiagSeverity::Error);
                    let has_warn  = snap.diagnostics.iter().any(|&(dl, _, _, ref s)| dl == li && *s == DiagSeverity::Warning);
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
                    if x + cw > 0 && x < clip_r {
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

                // Diagnostic squiggles
                for &(dline, cs, ce, ref sev) in &snap.diagnostics {
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
                draw_str(buf, w, h, g, &snap.find_query, qx, row1_base, FG, qclip);
                if snap.find_focus == FindFocus::Query {
                    let cx = qx + snap.find_query.chars().count() as i32 * cw;
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
                    draw_str(buf, w, h, g, &snap.find_repl, rx, row2_base, FG, rclip);
                    if snap.find_focus == FindFocus::Replace {
                        let cx = rx + snap.find_repl.chars().count() as i32 * cw;
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
                    if cell.bg != BG { fill(buf, w, h, cx, py, cw, lh, cell.bg); }
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
        if let Some(snap) = pane_snaps.iter().find(|p| p.is_active) {
            let dirty_mark = if snap.dirty { " *" } else { "" };
            draw_str(buf, w, h, g, &format!("  {}{dirty_mark}", snap.path_name), 0, sbase, FG, w as i32);
            let lc_str = format!("Ln {}, Col {}  ", snap.cur_line + 1, snap.cur_col + 1);
            let lc_w   = lc_str.chars().count() as i32 * cw;
            draw_str(buf, w, h, g, &lc_str, w as i32 - lc_w, sbase, FG_DIM, w as i32);
        }
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
    let el = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    let proxy = el.create_proxy();
    let mut app = App::new(file_arg, dir_arg, proxy);
    el.run_app(&mut app).unwrap();
}
