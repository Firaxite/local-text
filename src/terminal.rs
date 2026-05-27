// Terminal pane: PTY + VT100/ANSI grid + key encoding.
//
// One TermPane is created per terminal pane.  The PTY is set up via libc::forkpty
// (macOS / any POSIX system).  A background reader thread forwards raw bytes from
// the PTY master fd to the main event loop via EventLoopProxy<UserEvent>.
// The main thread calls feed_bytes() in user_event(), which runs the vte parser
// and updates the grid in-place.

use std::collections::VecDeque;
use std::ffi::CString;
use vte::{Perform, Parser};
use winit::event_loop::EventLoopProxy;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::UserEvent;

// ── Color palette defaults (TokyoNight) ──────────────────────────────────────
const DEFAULT_FG: u32 = 0xA9B1D6;
const DEFAULT_BG: u32 = 0x1A1B26;

// Standard 16 ANSI colors mapped to TokyoNight equivalents.
const ANSI16: [u32; 16] = [
    0x15161E, // 0  black
    0xF7768E, // 1  red
    0x9ECE6A, // 2  green
    0xE0AF68, // 3  yellow
    0x7AA2F7, // 4  blue
    0xBB9AF7, // 5  magenta
    0x7DCFFF, // 6  cyan
    0xA9B1D6, // 7  white
    0x414868, // 8  bright black
    0xF7768E, // 9  bright red
    0x9ECE6A, // 10 bright green
    0xE0AF68, // 11 bright yellow
    0x7AA2F7, // 12 bright blue
    0xBB9AF7, // 13 bright magenta
    0x7DCFFF, // 14 bright cyan
    0xC0CAF5, // 15 bright white
];

// ── Cell ─────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
pub struct Cell {
    pub ch:   char,
    pub fg:   u32,
    pub bg:   u32,
    pub bold: bool,
}

impl Default for Cell {
    fn default() -> Self { Cell { ch: ' ', fg: DEFAULT_FG, bg: DEFAULT_BG, bold: false } }
}

// ── Mouse reporting mode ──────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum MouseReportMode {
    #[default] None,
    X10,          // ?1000 — press/release only
    ButtonEvent,  // ?1002 — press, release, drag
    AnyEvent,     // ?1003 — all motion
}

// ── TermGrid ─────────────────────────────────────────────────────────────────
pub struct TermGrid {
    pub cols: usize,
    pub rows: usize,
    /// Active screen: flat row-major, index = row * cols + col.
    pub cells: Vec<Cell>,
    pub cur_col: usize,
    pub cur_row: usize,
    /// Scrollback: oldest at front, newest at back. VecDeque gives O(1) pop_front.
    pub scrollback: VecDeque<Vec<Cell>>,
    /// How many lines of scrollback the user has scrolled up (0 = live).
    pub scroll_offset: usize,
    /// Current SGR state applied to new cells.
    pub cur_fg: u32,
    pub cur_bg: u32,
    pub cur_bold: bool,
    pub cur_visible: bool,
    pub mouse_report: MouseReportMode,
    pub mouse_sgr:    bool,
    /// DECSTBM scrolling region (0-based, inclusive).
    pub scroll_top: usize,
    pub scroll_bot: usize,
    /// Alternate screen state (?1047/?1049).
    pub alt_cells:   Option<Vec<Cell>>,
    pub alt_cur_col: usize,
    pub alt_cur_row: usize,
    pub alt_scroll_top: usize,
    pub alt_scroll_bot: usize,
    /// Saved cursor position (\x1b7 / \x1b[s).
    pub saved_cur_col: usize,
    pub saved_cur_row: usize,
}

impl TermGrid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cells = vec![Cell::default(); cols * rows];
        let rows_m1 = rows.saturating_sub(1);
        TermGrid {
            cols, rows, cells, cur_col: 0, cur_row: 0,
            scrollback: VecDeque::new(), scroll_offset: 0,
            cur_fg: DEFAULT_FG, cur_bg: DEFAULT_BG, cur_bold: false, cur_visible: true,
            mouse_report: MouseReportMode::None, mouse_sgr: false,
            scroll_top: 0, scroll_bot: rows_m1,
            alt_cells: None, alt_cur_col: 0, alt_cur_row: 0,
            alt_scroll_top: 0, alt_scroll_bot: rows_m1,
            saved_cur_col: 0, saved_cur_row: 0,
        }
    }

    fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell { &mut self.cells[row * self.cols + col] }

    /// Scroll the scroll region up by one line. Only adds to scrollback when region is full-screen.
    fn scroll_up(&mut self) {
        // Defensive clamp: scroll_bot must never exceed rows-1.
        let scroll_bot = self.scroll_bot.min(self.rows.saturating_sub(1));
        if self.scroll_top == 0 && scroll_bot == self.rows.saturating_sub(1) {
            let row: Vec<Cell> = self.cells[0..self.cols].to_vec();
            if self.scrollback.len() >= 1000 { self.scrollback.pop_front(); }
            self.scrollback.push_back(row);
        }
        let top = self.scroll_top * self.cols;
        let bot = (scroll_bot + 1) * self.cols;
        if bot > top + self.cols {
            self.cells.copy_within(top + self.cols..bot, top);
        }
        let last = scroll_bot * self.cols;
        for c in &mut self.cells[last..last + self.cols] { *c = Cell::default(); }
    }

    /// Scroll the scroll region down by one line (for insert-line / reverse-index).
    fn scroll_down(&mut self) {
        // Defensive clamp: scroll_bot must never exceed rows-1.
        let scroll_bot = self.scroll_bot.min(self.rows.saturating_sub(1));
        let top = self.scroll_top * self.cols;
        let bot = (scroll_bot + 1) * self.cols;
        if bot > top + self.cols {
            self.cells.copy_within(top..bot - self.cols, top + self.cols);
        }
        for c in &mut self.cells[top..top + self.cols] { *c = Cell::default(); }
    }

    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        if new_cols == self.cols && new_rows == self.rows { return; }
        let mut new_cells = vec![Cell::default(); new_cols * new_rows];
        let copy_rows = self.rows.min(new_rows);
        let copy_cols = self.cols.min(new_cols);
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                new_cells[r * new_cols + c] = self.cells[r * self.cols + c];
            }
        }
        self.cells = new_cells;
        self.cols = new_cols;
        self.rows = new_rows;
        self.cur_row = self.cur_row.min(new_rows.saturating_sub(1));
        self.cur_col = self.cur_col.min(new_cols.saturating_sub(1));
        self.scroll_top = 0;
        self.scroll_bot = new_rows.saturating_sub(1);
        self.alt_cells = None; // discard alt screen on resize
    }

    /// Return the visible rows accounting for scroll_offset.
    pub fn visible_rows(&self) -> Vec<Vec<Cell>> {
        let offset = self.scroll_offset;
        if offset == 0 {
            (0..self.rows).map(|r| self.cells[r*self.cols..(r+1)*self.cols].to_vec()).collect()
        } else {
            let sb_len = self.scrollback.len();
            let start = sb_len.saturating_sub(offset);
            let mut result: Vec<Vec<Cell>> = Vec::with_capacity(self.rows);
            for row in self.scrollback.iter().skip(start) {
                if result.len() == self.rows { return result; }
                result.push(row.clone());
            }
            for r in 0..self.rows {
                if result.len() == self.rows { return result; }
                result.push(self.cells[r*self.cols..(r+1)*self.cols].to_vec());
            }
            result
        }
    }
}

// ── VteHandler ────────────────────────────────────────────────────────────────
struct VteHandler<'a> {
    grid: &'a mut TermGrid,
}

impl<'a> Perform for VteHandler<'a> {
    fn print(&mut self, c: char) {
        let g = &mut *self.grid;
        if g.cur_col >= g.cols {
            g.cur_col = 0;
            if g.cur_row == g.scroll_bot {
                g.scroll_up();
            } else {
                g.cur_row = (g.cur_row + 1).min(g.rows - 1);
            }
        }
        let (r, co) = (g.cur_row, g.cur_col);
        let (fg, bg, bold) = (g.cur_fg, g.cur_bg, g.cur_bold);
        let cell = g.cell_mut(r, co);
        cell.ch   = c;
        cell.fg   = fg;
        cell.bg   = bg;
        cell.bold = bold;
        g.cur_col += 1;
    }

    fn execute(&mut self, byte: u8) {
        let g = &mut *self.grid;
        match byte {
            b'\r' => { g.cur_col = 0; }
            b'\n' => {
                if g.cur_row == g.scroll_bot {
                    g.scroll_up();
                } else {
                    g.cur_row = (g.cur_row + 1).min(g.rows - 1);
                }
            }
            0x08 => { // backspace
                if g.cur_col > 0 { g.cur_col -= 1; }
            }
            b'\t' => {
                g.cur_col = ((g.cur_col / 8) + 1) * 8;
                if g.cur_col >= g.cols { g.cur_col = g.cols - 1; }
            }
            0x07 => {} // bell — ignore
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        let g = &mut *self.grid;
        // Collect numeric params; 0 where missing.
        let ps: Vec<u16> = params.iter().map(|p| p.first().copied().unwrap_or(0)).collect();
        let p0 = ps.first().copied().unwrap_or(0) as usize;
        let p1 = ps.get(1).copied().unwrap_or(0) as usize;
        match action {
            // Cursor up/down/forward/back
            'A' => { let n = p0.max(1); g.cur_row = g.cur_row.saturating_sub(n); }
            'B' => { let n = p0.max(1); g.cur_row = (g.cur_row + n).min(g.rows - 1); }
            'C' => { let n = p0.max(1); g.cur_col = (g.cur_col + n).min(g.cols - 1); }
            'D' => { let n = p0.max(1); g.cur_col = g.cur_col.saturating_sub(n); }
            // Cursor position
            'H' | 'f' => {
                g.cur_row = p0.saturating_sub(1).min(g.rows - 1);
                g.cur_col = p1.saturating_sub(1).min(g.cols - 1);
            }
            // Erase display
            'J' => match p0 {
                0 => { // clear from cursor to end of screen
                    let start = g.cur_row * g.cols + g.cur_col;
                    for c in &mut g.cells[start..] { *c = Cell::default(); }
                }
                1 => { // clear from start to cursor
                    let end = g.cur_row * g.cols + g.cur_col;
                    for c in &mut g.cells[..=end] { *c = Cell::default(); }
                }
                _ => { // 2 or 3: clear entire screen
                    for c in g.cells.iter_mut() { *c = Cell::default(); }
                    g.cur_row = 0; g.cur_col = 0;
                }
            }
            // Erase line
            'K' => match p0 {
                0 => { // clear from cursor to end of line
                    let start = g.cur_row * g.cols + g.cur_col;
                    let end   = g.cur_row * g.cols + g.cols;
                    for c in &mut g.cells[start..end] { *c = Cell::default(); }
                }
                1 => { // clear from start of line to cursor
                    let start = g.cur_row * g.cols;
                    let end   = g.cur_row * g.cols + g.cur_col;
                    for c in &mut g.cells[start..=end] { *c = Cell::default(); }
                }
                _ => { // 2: clear entire line
                    let start = g.cur_row * g.cols;
                    let end   = start + g.cols;
                    for c in &mut g.cells[start..end] { *c = Cell::default(); }
                }
            }
            // SGR — colors and attributes
            'm' => {
                let mut i = 0;
                while i < ps.len() {
                    match ps[i] {
                        0 => { g.cur_fg = DEFAULT_FG; g.cur_bg = DEFAULT_BG; g.cur_bold = false; }
                        1 => { g.cur_bold = true; }
                        22 => { g.cur_bold = false; }
                        39 => { g.cur_fg = DEFAULT_FG; }
                        49 => { g.cur_bg = DEFAULT_BG; }
                        n @ 30..=37 => { g.cur_fg = ANSI16[(n - 30) as usize]; }
                        n @ 90..=97 => { g.cur_fg = ANSI16[(n - 90 + 8) as usize]; }
                        n @ 40..=47 => { g.cur_bg = ANSI16[(n - 40) as usize]; }
                        n @ 100..=107 => { g.cur_bg = ANSI16[(n - 100 + 8) as usize]; }
                        38 => {
                            if ps.get(i+1).copied() == Some(5) && i + 2 < ps.len() {
                                g.cur_fg = ansi256(ps[i+2] as u8); i += 2;
                            } else if ps.get(i+1).copied() == Some(2) && i + 4 < ps.len() {
                                g.cur_fg = rgb(ps[i+2] as u8, ps[i+3] as u8, ps[i+4] as u8); i += 4;
                            }
                        }
                        48 => {
                            if ps.get(i+1).copied() == Some(5) && i + 2 < ps.len() {
                                g.cur_bg = ansi256(ps[i+2] as u8); i += 2;
                            } else if ps.get(i+1).copied() == Some(2) && i + 4 < ps.len() {
                                g.cur_bg = rgb(ps[i+2] as u8, ps[i+3] as u8, ps[i+4] as u8); i += 4;
                            }
                        }
                        7 => { // reverse video
                            let tmp = g.cur_fg; g.cur_fg = g.cur_bg; g.cur_bg = tmp;
                        }
                        27 => { // reverse off — reset to defaults
                            g.cur_fg = DEFAULT_FG; g.cur_bg = DEFAULT_BG;
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            'h' | 'l' => {
                let enable = action == 'h';
                if intermediates.first() == Some(&0x3F) {
                    for &p in ps.iter() {
                        match p {
                            25   => g.cur_visible = enable,
                            1000 => g.mouse_report = if enable { MouseReportMode::X10 } else { MouseReportMode::None },
                            1002 => g.mouse_report = if enable { MouseReportMode::ButtonEvent } else { MouseReportMode::None },
                            1003 => g.mouse_report = if enable { MouseReportMode::AnyEvent } else { MouseReportMode::None },
                            1006 => g.mouse_sgr = enable,
                            47 | 1047 | 1049 => {
                                if enable {
                                    if g.alt_cells.is_none() {
                                        g.alt_cells = Some(g.cells.clone());
                                        g.alt_cur_col = g.cur_col;
                                        g.alt_cur_row = g.cur_row;
                                        g.alt_scroll_top = g.scroll_top;
                                        g.alt_scroll_bot = g.scroll_bot;
                                        for c in g.cells.iter_mut() { *c = Cell::default(); }
                                        g.cur_col = 0; g.cur_row = 0;
                                        g.scroll_top = 0;
                                        g.scroll_bot = g.rows.saturating_sub(1);
                                    }
                                } else if let Some(saved) = g.alt_cells.take() {
                                    g.cells = saved;
                                    g.cur_col = g.alt_cur_col;
                                    g.cur_row = g.alt_cur_row;
                                    g.scroll_top = g.alt_scroll_top;
                                    // Clamp restored scroll_bot — it may be stale if the grid
                                    // was resized while the alt screen was active.
                                    g.scroll_bot = g.alt_scroll_bot.min(g.rows.saturating_sub(1));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            // Column/line position
            'G' => { g.cur_col = p0.saturating_sub(1).min(g.cols - 1); }
            'd' => { g.cur_row = p0.saturating_sub(1).min(g.rows - 1); }
            // Scroll up / down N lines within scroll region
            'S' => { let n = p0.max(1); for _ in 0..n { g.scroll_up(); } }
            'T' => { let n = p0.max(1); for _ in 0..n { g.scroll_down(); } }
            // DECSTBM — set scrolling region
            'r' => {
                let top = p0.saturating_sub(1).min(g.rows - 1);
                let bot = if p1 == 0 { g.rows - 1 } else { (p1 - 1).min(g.rows - 1) };
                if top < bot {
                    g.scroll_top = top;
                    g.scroll_bot = bot;
                } else {
                    g.scroll_top = 0;
                    g.scroll_bot = g.rows.saturating_sub(1);
                }
                g.cur_row = 0; g.cur_col = 0;
            }
            // Insert / delete lines at cursor
            'L' => {
                let eff_bot = g.scroll_bot.min(g.rows.saturating_sub(1));
                let n = p0.max(1).min(eff_bot.saturating_sub(g.cur_row) + 1);
                let src_end = eff_bot.saturating_sub(n - 1) * g.cols;
                let dst_start = (g.cur_row + n) * g.cols;
                let src_start = g.cur_row * g.cols;
                if src_end > src_start {
                    g.cells.copy_within(src_start..src_end, dst_start);
                }
                for r in g.cur_row..g.cur_row + n {
                    let s = r * g.cols;
                    for c in &mut g.cells[s..s + g.cols] { *c = Cell::default(); }
                }
            }
            'M' => {
                let eff_bot = g.scroll_bot.min(g.rows.saturating_sub(1));
                let n = p0.max(1).min(eff_bot.saturating_sub(g.cur_row) + 1);
                let src_start = (g.cur_row + n) * g.cols;
                let src_end = (eff_bot + 1) * g.cols;
                let dst_start = g.cur_row * g.cols;
                if src_end > src_start {
                    g.cells.copy_within(src_start..src_end, dst_start);
                }
                let clear_start = (eff_bot + 1 - n) * g.cols;
                let clear_end = (eff_bot + 1) * g.cols;
                for c in &mut g.cells[clear_start..clear_end] { *c = Cell::default(); }
            }
            // Delete / erase / insert characters in current line
            'P' => {
                let n = p0.max(1).min(g.cols.saturating_sub(g.cur_col));
                let row = g.cur_row * g.cols;
                let src = row + g.cur_col + n;
                if src < row + g.cols {
                    g.cells.copy_within(src..row + g.cols, row + g.cur_col);
                }
                let clear = row + g.cols - n;
                for c in &mut g.cells[clear..row + g.cols] { *c = Cell::default(); }
            }
            'X' => {
                let n = p0.max(1).min(g.cols.saturating_sub(g.cur_col));
                let start = g.cur_row * g.cols + g.cur_col;
                for c in &mut g.cells[start..start + n] { *c = Cell::default(); }
            }
            '@' => {
                let n = p0.max(1).min(g.cols.saturating_sub(g.cur_col));
                let row = g.cur_row * g.cols;
                let src_end = row + g.cols - n;
                if src_end > row + g.cur_col {
                    g.cells.copy_within(row + g.cur_col..src_end, row + g.cur_col + n);
                }
                for c in &mut g.cells[row + g.cur_col..row + g.cur_col + n] { *c = Cell::default(); }
            }
            // Cursor save / restore
            's' => { g.saved_cur_col = g.cur_col; g.saved_cur_row = g.cur_row; }
            'u' => {
                g.cur_col = g.saved_cur_col.min(g.cols.saturating_sub(1));
                g.cur_row = g.saved_cur_row.min(g.rows.saturating_sub(1));
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let _ = params;
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        let g = &mut *self.grid;
        match byte {
            b'7' => { g.saved_cur_col = g.cur_col; g.saved_cur_row = g.cur_row; }
            b'8' => {
                g.cur_col = g.saved_cur_col.min(g.cols.saturating_sub(1));
                g.cur_row = g.saved_cur_row.min(g.rows.saturating_sub(1));
            }
            b'M' => { // reverse index — scroll down if at top of scroll region
                if g.cur_row == g.scroll_top {
                    g.scroll_down();
                } else if g.cur_row > 0 {
                    g.cur_row -= 1;
                }
            }
            _ => {}
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> u32 { ((r as u32) << 16) | ((g as u32) << 8) | b as u32 }

fn ansi256(n: u8) -> u32 {
    match n {
        0..=15 => ANSI16[n as usize],
        16..=231 => {
            let n = n - 16;
            let r = (n / 36) * 51;
            let g = ((n % 36) / 6) * 51;
            let b = (n % 6) * 51;
            rgb(r, g, b)
        }
        232..=255 => {
            let v = (n - 232) * 10 + 8;
            rgb(v, v, v)
        }
    }
}

// ── TermPane ──────────────────────────────────────────────────────────────────
pub struct TermPane {
    pub id:      usize,
    pub grid:    TermGrid,
    parser:      Parser,
    pub pty_fd:  i32,
    pub _reader: std::thread::JoinHandle<()>,
    pub title:   String,
    pub shell:   String,
}

impl Drop for TermPane {
    fn drop(&mut self) {
        if self.pty_fd >= 0 {
            // SAFETY: pty_fd is a valid open file descriptor owned by this TermPane.
            unsafe { libc::close(self.pty_fd); }
        }
    }
}

/// Parse raw PTY bytes and update the grid.
pub fn feed_bytes(pane: &mut TermPane, data: &[u8]) {
    let mut handler = VteHandler { grid: &mut pane.grid };
    for &b in data {
        pane.parser.advance(&mut handler, b);
    }
}

/// Resize the PTY and grid when the pane area changes.
pub fn resize_pty(pane: &mut TermPane, cols: usize, rows: usize) {
    if cols == pane.grid.cols && rows == pane.grid.rows { return; }
    let ws = libc::winsize { ws_col: cols as u16, ws_row: rows as u16, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: pty_fd is a valid open PTY master fd; ws is a correctly-sized winsize struct.
    unsafe { libc::ioctl(pane.pty_fd, libc::TIOCSWINSZ, &ws); }
    pane.grid.resize(cols, rows);
}

/// Fork a PTY, exec $SHELL (or shell_override) in the child, and start a reader thread.
#[cfg(unix)]
pub fn spawn_terminal(pane_id: usize, cols: usize, rows: usize,
    proxy: EventLoopProxy<UserEvent>) -> TermPane {
    spawn_terminal_with_shell(pane_id, cols, rows, proxy, None)
}

#[cfg(unix)]
pub fn spawn_terminal_with_shell(pane_id: usize, cols: usize, rows: usize,
    proxy: EventLoopProxy<UserEvent>, shell_override: Option<String>) -> TermPane {
    use std::env;
    use std::ptr;

    // Track whether we got an explicit override so we can choose the right exec strategy.
    let is_override = shell_override.is_some();
    let shell = shell_override
        .or_else(|| env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/sh".to_owned());

    let mut ws = libc::winsize {
        ws_col: cols as u16,
        ws_row: rows as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let mut master_fd: libc::c_int = -1;
    // SAFETY: forkpty is a POSIX API; master_fd is out-param, ws is valid winsize.
    let pid = unsafe {
        libc::forkpty(&mut master_fd, ptr::null_mut(), ptr::null_mut(), &mut ws)
    };
    assert!(pid >= 0, "forkpty failed: {}", std::io::Error::last_os_error());

    if pid == 0 {
        // SAFETY: we are in the forked child process; the NUL-terminated literals and
        // argv array are valid for the duration of these calls. execvp replaces the
        // process image so no cleanup is needed on success; exit(1) handles failure.
        unsafe {
            libc::setenv(
                b"TERM\0".as_ptr().cast(),
                b"xterm-256color\0".as_ptr().cast(),
                1,
            );
            if is_override {
                // The override is a full command string with arguments (e.g.
                // "ssh -o ControlPath=... user@host"). execvp does NOT perform
                // shell word-splitting — it would try to find a file literally
                // named the whole string, which doesn't exist. Delegate to
                // `sh -c <cmd>` so the shell parses the arguments correctly.
                let sh_c   = CString::new("sh").unwrap();
                let flag_c = CString::new("-c").unwrap();
                let cmd_c  = CString::new(shell.as_str()).unwrap();
                libc::execvp(
                    sh_c.as_ptr(),
                    [sh_c.as_ptr(), flag_c.as_ptr(), cmd_c.as_ptr(), ptr::null()].as_ptr(),
                );
            } else {
                // Plain shell path (e.g. "/bin/zsh") — exec directly with no extra args.
                let shell_c = CString::new(shell.as_str()).unwrap();
                libc::execvp(shell_c.as_ptr(), [shell_c.as_ptr(), ptr::null()].as_ptr());
            }
            libc::exit(1);
        }
    }

    // Parent: spawn reader thread
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: master_fd is a valid open PTY master fd; buf is a mutable
            // stack-allocated slice sized to match the length argument.
            let n = unsafe { libc::read(master_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                let _ = proxy.send_event(UserEvent::Redraw);
                break;
            }
            // Box<[u8]> carries exactly n bytes with no excess capacity.
            let data: Box<[u8]> = buf[..n as usize].into();
            if proxy.send_event(UserEvent::TermOutput { pane_id, data }).is_err() { break; }
        }
    });

    TermPane {
        id: pane_id,
        grid: TermGrid::new(cols, rows),
        parser: Parser::new(),
        pty_fd: master_fd,
        _reader: reader,
        title: "Terminal".to_owned(),
        shell,
    }
}

// ── Key encoding ──────────────────────────────────────────────────────────────

/// Encode a winit keyboard event into bytes to write to the PTY master fd.
/// Returns None for events that should not be forwarded.
pub fn encode_key(key: &Key, mods: ModifiersState, text: Option<&str>) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    match key {
        Key::Named(NamedKey::Enter)      => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace)  => Some(b"\x7f".to_vec()),
        Key::Named(NamedKey::Escape)     => Some(b"\x1b".to_vec()),
        Key::Named(NamedKey::Tab)        => {
            if mods.shift_key() { Some(b"\x1b[Z".to_vec()) } else { Some(b"\t".to_vec()) }
        }
        Key::Named(NamedKey::ArrowUp)    => Some(b"\x1b[A".to_vec()),
        Key::Named(NamedKey::ArrowDown)  => Some(b"\x1b[B".to_vec()),
        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
        Key::Named(NamedKey::ArrowLeft)  => Some(b"\x1b[D".to_vec()),
        Key::Named(NamedKey::Home)       => Some(b"\x1b[H".to_vec()),
        Key::Named(NamedKey::End)        => Some(b"\x1b[F".to_vec()),
        Key::Named(NamedKey::Delete)     => Some(b"\x1b[3~".to_vec()),
        Key::Named(NamedKey::PageUp)     => Some(b"\x1b[5~".to_vec()),
        Key::Named(NamedKey::PageDown)   => Some(b"\x1b[6~".to_vec()),
        Key::Character(s) if ctrl => {
            let c = s.chars().next()?;
            if c.is_ascii_alphabetic() {
                Some(vec![(c.to_ascii_lowercase() as u8) - b'a' + 1])
            } else if c == '[' {
                Some(b"\x1b".to_vec())
            } else {
                None
            }
        }
        _ => text.filter(|t| !t.is_empty()).map(|t| t.as_bytes().to_vec()),
    }
}

/// Encode a mouse event as bytes to write to the PTY master fd.
/// col/row are 0-based grid coordinates.
/// cb is the button code (0=left, 1=mid, 2=right, 3=release(X10), 32+n=motion,
///    64=scroll-up, 65=scroll-down); modifier bits already OR'd in by caller.
/// press=false only matters for SGR mode (uses 'm' suffix for release).
pub fn encode_mouse(col: usize, row: usize, cb: u8, press: bool, sgr: bool) -> Vec<u8> {
    let cx = col + 1;
    let cy = row + 1;
    if sgr {
        let suffix = if press { 'M' } else { 'm' };
        format!("\x1b[<{};{};{}{}", cb, cx, cy, suffix).into_bytes()
    } else {
        let b_cb = (cb as usize + 32).min(255) as u8;
        let b_cx = (cx + 32).min(255) as u8;
        let b_cy = (cy + 32).min(255) as u8;
        vec![0x1b, b'[', b'M', b_cb, b_cx, b_cy]
    }
}
