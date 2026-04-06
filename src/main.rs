mod platform;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use fontdue::{Font, FontSettings, Metrics};
use ropey::Rope;
use winit::application::ApplicationHandler;
use std::time::{Duration, Instant};
use winit::event::{ElementState, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowAttributes};
#[cfg(target_os = "macos")]
use winit::platform::macos::WindowAttributesExtMacOS;

// ── TokyoNight palette (0x00RRGGBB) ──────────────────────────────────────────
const BG:     u32 = 0x1A1B26;
const BG2:    u32 = 0x24283B;
const FG:     u32 = 0xA9B1D6;
const FG_DIM: u32 = 0x565F89;
const ACCENT: u32 = 0x7AA2F7;

// ── Layout ────────────────────────────────────────────────────────────────────
const FONT_PX:  f32 = 14.0;
const STATUS_H: i32 = 22;
const ED_LPAD:  i32 = 6;

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
        let (m, _) = font.rasterize('M', px);
        let cw  = m.advance_width.ceil() as i32;
        let lh  = (px * 1.5).ceil() as i32;
        let asc = (px * 1.1).ceil() as i32;
        let mut s = Self { font, px, map: HashMap::new(), cw, lh, asc };
        for ch in ' '..='~' { s.load(ch); }
        s
    }

    fn load(&mut self, ch: char) {
        self.map.entry(ch).or_insert_with(|| self.font.rasterize(ch, self.px));
    }

    fn get(&self, ch: char) -> Option<(&Metrics, &[u8])> {
        self.map.get(&ch).map(|(m, b)| (m, b.as_slice()))
    }
}

// ── Application state ─────────────────────────────────────────────────────────
struct State {
    win:      Arc<Window>,
    renderer: platform::Renderer,
    w: u32, h: u32,
    text:   Rope,
    path:   Option<PathBuf>,
    dirty:  bool,
    cursor:         usize,
    cursor_visible: bool,
    cursor_blink:   Instant,
    scroll: usize,
    mods:   ModifiersState,
    glyphs: Glyphs,
}

impl State {
    fn editor_h(&self) -> i32 { self.h as i32 - STATUS_H }

    fn cursor_lc(&self) -> (usize, usize) {
        let c    = self.cursor.min(self.text.len_chars());
        let line = self.text.char_to_line(c);
        let col  = c - self.text.line_to_char(line);
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
        let vis = (self.editor_h() / self.glyphs.lh).max(1) as usize;
        let (line, _) = self.cursor_lc();
        if line < self.scroll { self.scroll = line; }
        if line >= self.scroll + vis { self.scroll = line + 1 - vis; }
    }

    // ── Editing ───────────────────────────────────────────────────────────────
    fn insert_str(&mut self, s: &str) {
        let c = self.cursor.min(self.text.len_chars());
        self.text.insert(c, s);
        self.cursor = c + s.chars().count();
        self.dirty = true;
        self.ensure_visible();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 { return; }
        let c = self.cursor.min(self.text.len_chars());
        self.text.remove(c - 1..c);
        self.cursor = c - 1;
        self.dirty = true;
        self.ensure_visible();
    }

    fn delete_fwd(&mut self) {
        let c = self.cursor.min(self.text.len_chars());
        if c >= self.text.len_chars() { return; }
        self.text.remove(c..c + 1);
        self.dirty = true;
    }

    // ── Cursor movement ───────────────────────────────────────────────────────
    fn move_left(&mut self) {
        if self.cursor > 0 { self.cursor -= 1; }
        self.ensure_visible();
    }

    fn move_right(&mut self) {
        if self.cursor < self.text.len_chars() { self.cursor += 1; }
        self.ensure_visible();
    }

    fn move_up(&mut self) {
        let (line, col) = self.cursor_lc();
        if line == 0 { self.cursor = 0; self.ensure_visible(); return; }
        let prev = line - 1;
        self.cursor = self.text.line_to_char(prev) + col.min(Self::line_len(&self.text, prev));
        self.ensure_visible();
    }

    fn move_down(&mut self) {
        let (line, col) = self.cursor_lc();
        let last = Self::last_line(&self.text);
        if line >= last { self.cursor = self.text.len_chars(); self.ensure_visible(); return; }
        let next = line + 1;
        self.cursor = self.text.line_to_char(next) + col.min(Self::line_len(&self.text, next));
        self.ensure_visible();
    }

    fn move_home(&mut self) {
        let (line, _) = self.cursor_lc();
        self.cursor = self.text.line_to_char(line);
        self.ensure_visible();
    }

    fn move_end(&mut self) {
        let (line, _) = self.cursor_lc();
        self.cursor = self.text.line_to_char(line) + Self::line_len(&self.text, line);
        self.ensure_visible();
    }

    fn reset_blink(&mut self) {
        self.cursor_visible = true;
        self.cursor_blink = Instant::now() + Duration::from_millis(500);
    }

    fn is_word_char(c: char) -> bool { c.is_alphanumeric() || c == '_' }

    fn move_word_left(&mut self) {
        let mut pos = self.cursor.min(self.text.len_chars());
        while pos > 0 && !Self::is_word_char(self.text.char(pos - 1)) { pos -= 1; }
        while pos > 0 &&  Self::is_word_char(self.text.char(pos - 1)) { pos -= 1; }
        self.cursor = pos;
        self.ensure_visible();
    }

    fn move_word_right(&mut self) {
        let len = self.text.len_chars();
        let mut pos = self.cursor.min(len);
        while pos < len && !Self::is_word_char(self.text.char(pos)) { pos += 1; }
        while pos < len &&  Self::is_word_char(self.text.char(pos)) { pos += 1; }
        self.cursor = pos;
        self.ensure_visible();
    }

    fn move_doc_start(&mut self) { self.cursor = 0; self.ensure_visible(); }
    fn move_doc_end(&mut self)   { self.cursor = self.text.len_chars(); self.ensure_visible(); }

    fn delete_word_back(&mut self) {
        let end = self.cursor.min(self.text.len_chars());
        let mut start = end;
        while start > 0 && !Self::is_word_char(self.text.char(start - 1)) { start -= 1; }
        while start > 0 &&  Self::is_word_char(self.text.char(start - 1)) { start -= 1; }
        if start < end {
            self.text.remove(start..end);
            self.cursor = start;
            self.dirty = true;
            self.ensure_visible();
        }
    }

    fn delete_to_line_start(&mut self) {
        let end = self.cursor.min(self.text.len_chars());
        let (line, _) = self.cursor_lc();
        let start = self.text.line_to_char(line);
        if start < end {
            self.text.remove(start..end);
            self.cursor = start;
            self.dirty = true;
            self.ensure_visible();
        }
    }

    fn delete_word_fwd(&mut self) {
        let len = self.text.len_chars();
        let start = self.cursor.min(len);
        let mut end = start;
        while end < len && !Self::is_word_char(self.text.char(end)) { end += 1; }
        while end < len &&  Self::is_word_char(self.text.char(end)) { end += 1; }
        if end > start {
            self.text.remove(start..end);
            self.dirty = true;
        }
    }

    fn delete_to_line_end(&mut self) {
        let len = self.text.len_chars();
        let start = self.cursor.min(len);
        let (line, _) = self.cursor_lc();
        let line_end = self.text.line_to_char(line) + Self::line_len(&self.text, line);
        if line_end > start {
            self.text.remove(start..line_end);
            self.dirty = true;
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        if delta < 0 {
            self.scroll = self.scroll.saturating_sub((-delta) as usize);
        } else {
            self.scroll = (self.scroll + delta as usize).min(Self::last_line(&self.text));
        }
    }

    // ── File I/O ──────────────────────────────────────────────────────────────
    fn load_file(&mut self, path: PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                self.text   = Rope::from_str(&content);
                self.path   = Some(path);
                self.cursor = 0;
                self.scroll = 0;
                self.dirty  = false;
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

// ── App ───────────────────────────────────────────────────────────────────────
struct App {
    state:    Option<State>,
    file_arg: Option<PathBuf>,
}

impl App {
    fn new(file_arg: Option<PathBuf>) -> Self {
        Self { state: None, file_arg }
    }
}

impl ApplicationHandler for App {
    fn new_events(&mut self, _el: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            if let Some(s) = self.state.as_mut() {
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

        let font_bytes = include_bytes!("../assets/JetBrainsMono-Regular.ttf");
        let glyphs = Glyphs::new(font_bytes, FONT_PX);

        let sz = win.inner_size();
        let mut s = State {
            win,
            renderer,
            w: sz.width,
            h: sz.height,
            text:   Rope::new(),
            path:   None,
            dirty:  false,
            cursor:         0,
            cursor_visible: true,
            cursor_blink:   Instant::now() + Duration::from_millis(500),
            scroll: 0,
            mods:   ModifiersState::default(),
            glyphs,
        };

        if let Some(path) = self.file_arg.take() {
            s.load_file(path);
        }

        s.win.request_redraw();
        self.state = Some(s);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: winit::window::WindowId, event: WindowEvent) {
        let Some(s) = self.state.as_mut() else { return };

        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::Resized(sz) => {
                s.w = sz.width;
                s.h = sz.height;
                s.renderer.resize(sz.width, sz.height);
                s.win.request_redraw();
            }

            WindowEvent::ModifiersChanged(m) => {
                s.mods = m.state();
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y as i32,
                    MouseScrollDelta::PixelDelta(p)   => -(p.y as i32) / s.glyphs.lh,
                };
                if lines != 0 { s.scroll_by(lines); render(s); }
            }

            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let ctrl = s.mods.control_key();
                let cmd  = s.mods.super_key();
                let alt  = s.mods.alt_key();

                if (ctrl || cmd) && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "s") {
                    s.save();
                    render(s);
                } else if ctrl && matches!(&event.logical_key, Key::Character(_)) {
                    // other Ctrl+letter combos: ignore
                } else {
                    match &event.logical_key {
                        Key::Named(NamedKey::ArrowLeft) => {
                            if cmd            { s.move_home(); }
                            else if alt||ctrl { s.move_word_left(); }
                            else              { s.move_left(); }
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            if cmd            { s.move_end(); }
                            else if alt||ctrl { s.move_word_right(); }
                            else              { s.move_right(); }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if cmd { s.move_doc_start(); } else { s.move_up(); }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if cmd { s.move_doc_end(); } else { s.move_down(); }
                        }
                        Key::Named(NamedKey::Home) => {
                            if ctrl { s.move_doc_start(); } else { s.move_home(); }
                        }
                        Key::Named(NamedKey::End) => {
                            if ctrl { s.move_doc_end(); } else { s.move_end(); }
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
                    render(s);
                }
            }

            WindowEvent::RedrawRequested => render(s),

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

    // Snapshot values we need inside the closure (borrows must not alias s).
    let (cur_line, cur_col) = s.cursor_lc();
    let scroll   = s.scroll;
    let editor_h = s.editor_h();
    let lh       = s.glyphs.lh;
    let asc      = s.glyphs.asc;
    let cw       = s.glyphs.cw;
    let total          = s.text.len_lines();
    let vis            = (editor_h / lh).max(1) as usize;
    let cursor_visible = s.cursor_visible;

    // ── Build line snapshot (borrow rope, then release before render_frame) ──
    // Collect only what we need for the visible range to avoid holding a borrow
    // on `s` inside the rendering closure.
    let line_count = vis.min(total.saturating_sub(scroll));
    let mut lines: Vec<String> = Vec::with_capacity(line_count);
    for vi in 0..line_count {
        let li = scroll + vi;
        let line = s.text.line(li);
        let text: String = line.chars().take_while(|&c| c != '\n' && c != '\r').collect();
        lines.push(text);
    }

    let path_name = s.path.as_deref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("[no file]")
        .to_owned();
    let dirty = s.dirty;
    // Clone glyph cache ref out — we need it inside the closure which also
    // borrows renderer mutably.  We pass references to the Glyphs fields.
    let glyphs = &s.glyphs as *const Glyphs;

    s.renderer.render_frame(move |buf, w, h| {
        let g = unsafe { &*glyphs };

        // ── Clear ─────────────────────────────────────────────────────────
        for p in buf.iter_mut() { *p = BG; }

        // ── Editor lines ──────────────────────────────────────────────────
        for (vi, text) in lines.iter().enumerate() {
            let li       = scroll + vi;
            let py       = vi as i32 * lh;
            let baseline = py + asc;

            // Line text
            let mut x = ED_LPAD;
            for ch in text.chars() {
                if x + cw > 0 && x < w as i32 {
                    if let Some((m, bmap)) = g.get(ch) {
                        blit(buf, w, h, bmap, m, x, baseline, FG);
                    }
                }
                x += cw;
                if x >= w as i32 { break; }
            }

            // Thin blinking cursor (2px wide, drawn on top of text)
            if li == cur_line && cursor_visible {
                let cx = ED_LPAD + cur_col as i32 * cw;
                fill(buf, w, h, cx, py, 2, lh, ACCENT);
            }
        }

        // ── Status bar ────────────────────────────────────────────────────
        let sy = h as i32 - STATUS_H;
        fill(buf, w, h, 0, sy, w as i32, STATUS_H, BG2);
        fill(buf, w, h, 0, sy, w as i32, 1, 0x3B4261);

        let sbase = sy + STATUS_H * 3 / 4;
        let dirty_mark = if dirty { " *" } else { "" };
        let name_str = format!("  {path_name}{dirty_mark}");
        draw_str(buf, w, h, g, &name_str, 0, sbase, FG, w as i32);

        let lc_str = format!("Ln {}, Col {}  ", cur_line + 1, cur_col + 1);
        let lc_w = lc_str.chars().count() as i32 * cw;
        draw_str(buf, w, h, g, &lc_str, w as i32 - lc_w, sbase, FG_DIM, w as i32);
    });
}

// ── Entry point ───────────────────────────────────────────────────────────────
fn main() {
    let file_arg = std::env::args().nth(1).map(PathBuf::from);
    let el = EventLoop::new().unwrap();
    let mut app = App::new(file_arg);
    el.run_app(&mut app).unwrap();
}
