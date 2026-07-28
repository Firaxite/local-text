// Terminal pane: PTY + VT100/ANSI grid + key encoding.
//
// One TermPane is created per terminal pane.  The PTY is set up via libc::forkpty
// (macOS / any POSIX system).  A background reader thread forwards raw bytes from
// the PTY master fd to the main event loop via EventLoopProxy<UserEvent>.
// The main thread calls feed_bytes() in user_event(), which runs the vte parser
// and updates the grid in-place.

use std::collections::VecDeque;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    /// DEC application cursor-key mode (?1h). Programs such as interactive
    /// Python/readline expect SS3 arrow sequences while this is enabled.
    pub application_cursor: bool,
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
    /// Last CWD reported via OSC 7 (file://hostname/path).
    pub cwd: Option<std::path::PathBuf>,
}

impl TermGrid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cells = vec![Cell::default(); cols * rows];
        let rows_m1 = rows.saturating_sub(1);
        TermGrid {
            cols, rows, cells, cur_col: 0, cur_row: 0,
            scrollback: VecDeque::new(), scroll_offset: 0,
            cur_fg: DEFAULT_FG, cur_bg: DEFAULT_BG, cur_bold: false, cur_visible: true,
            application_cursor: false,
            mouse_report: MouseReportMode::None, mouse_sgr: false,
            scroll_top: 0, scroll_bot: rows_m1,
            alt_cells: None, alt_cur_col: 0, alt_cur_row: 0,
            alt_scroll_top: 0, alt_scroll_bot: rows_m1,
            saved_cur_col: 0, saved_cur_row: 0,
            cwd: None,
        }
    }

    fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        // Clamp defensively: the cursor can transiently sit one past the last column
        // (deferred wrap), and a malformed resize could desync row/col from the grid.
        let idx = (row * self.cols + col).min(self.cells.len().saturating_sub(1));
        &mut self.cells[idx]
    }

    /// Flat cell index for (row, col), clamped to `[0, cells.len()]` so it is always a
    /// valid *exclusive* range endpoint. Erase handlers build ranges from cursor
    /// positions that may sit one past the last column, so they must clamp first.
    #[inline]
    fn flat(&self, row: usize, col: usize) -> usize {
        (row * self.cols + col).min(self.cells.len())
    }

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
        let (cells, cur_col, cur_row, overflow) = resize_screen(
            &self.cells, self.cols, self.rows, self.cur_col, self.cur_row, new_cols, new_rows,
        );
        self.cells = cells;
        self.cur_col = cur_col;
        self.cur_row = cur_row;

        // Rows displaced by a smaller terminal remain available in scrollback.
        // This is deliberately bounded by the same cap used during normal output.
        if self.alt_cells.is_none() {
            for row in overflow {
                if self.scrollback.len() >= 1000 { self.scrollback.pop_front(); }
                self.scrollback.push_back(row);
            }
        }

        // While the alternate screen is active, alt_cells stores the primary
        // screen. Resize it too instead of dropping it and erasing the session.
        if let Some(saved) = self.alt_cells.take() {
            let (saved, saved_col, saved_row, saved_overflow) = resize_screen(
                &saved, self.cols, self.rows, self.alt_cur_col, self.alt_cur_row,
                new_cols, new_rows,
            );
            for row in saved_overflow {
                if self.scrollback.len() >= 1000 { self.scrollback.pop_front(); }
                self.scrollback.push_back(row);
            }
            self.alt_cells = Some(saved);
            self.alt_cur_col = saved_col;
            self.alt_cur_row = saved_row;
            self.alt_scroll_top = 0;
            self.alt_scroll_bot = new_rows.saturating_sub(1);
        }

        self.cols = new_cols;
        self.rows = new_rows;
        self.scroll_top = 0;
        self.scroll_bot = new_rows.saturating_sub(1);
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

/// Rewrap a screen without losing rows that no longer fit. Each physical source
/// row remains a hard line; only non-blank content beyond the new width wraps.
fn resize_screen(
    cells: &[Cell],
    old_cols: usize,
    old_rows: usize,
    cur_col: usize,
    cur_row: usize,
    new_cols: usize,
    new_rows: usize,
) -> (Vec<Cell>, usize, usize, Vec<Vec<Cell>>) {
    if new_cols == 0 || new_rows == 0 {
        return (vec![], 0, 0, vec![]);
    }

    let mut logical_rows: Vec<Vec<Cell>> = Vec::new();
    let mut mapped_cursor = (0usize, 0usize);
    let last_content_row = (0..old_rows).rev().find(|&row| {
        let start = row.saturating_mul(old_cols).min(cells.len());
        let end = start.saturating_add(old_cols).min(cells.len());
        cells[start..end].iter().any(|cell| cell.ch != ' ')
    }).unwrap_or(0);
    let meaningful_rows = last_content_row.max(cur_row).saturating_add(1).min(old_rows);
    for row in 0..meaningful_rows {
        let start = row.saturating_mul(old_cols).min(cells.len());
        let end = start.saturating_add(old_cols).min(cells.len());
        let source = &cells[start..end];
        let last_nonblank = source.iter().rposition(|c| c.ch != ' ').map_or(0, |i| i + 1);
        let used = if row == cur_row {
            last_nonblank.max(cur_col.saturating_add(1).min(old_cols))
        } else {
            last_nonblank
        };
        let chunks = used.max(1).div_ceil(new_cols);
        let base = logical_rows.len();
        for chunk in 0..chunks {
            let mut out = vec![Cell::default(); new_cols];
            let from = chunk * new_cols;
            let to = (from + new_cols).min(source.len()).min(used);
            if to > from {
                out[..to - from].copy_from_slice(&source[from..to]);
            }
            logical_rows.push(out);
        }
        if row == cur_row {
            mapped_cursor = (base + cur_col / new_cols, cur_col % new_cols);
        }
    }

    if logical_rows.is_empty() {
        logical_rows.push(vec![Cell::default(); new_cols]);
    }
    let screen_start = logical_rows.len().saturating_sub(new_rows);
    let overflow = logical_rows[..screen_start].to_vec();
    let visible = &logical_rows[screen_start..];
    let mut new_cells = vec![Cell::default(); new_cols * new_rows];
    for (row, source) in visible.iter().enumerate() {
        new_cells[row * new_cols..(row + 1) * new_cols].copy_from_slice(source);
    }
    let new_cur_row = mapped_cursor.0.saturating_sub(screen_start).min(new_rows - 1);
    let new_cur_col = mapped_cursor.1.min(new_cols - 1);
    (new_cells, new_cur_col, new_cur_row, overflow)
}

// ── VteHandler ────────────────────────────────────────────────────────────────
struct VteHandler<'a> {
    grid: &'a mut TermGrid,
}

impl<'a> Perform for VteHandler<'a> {
    fn print(&mut self, c: char) {
        let g = &mut *self.grid;
        if g.cells.is_empty() { return; } // degenerate 0×0 grid — nothing to draw into
        if g.cur_col >= g.cols {
            g.cur_col = 0;
            if g.cur_row == g.scroll_bot {
                g.scroll_up();
            } else {
                g.cur_row = (g.cur_row + 1).min(g.rows.saturating_sub(1));
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
                    g.cur_row = (g.cur_row + 1).min(g.rows.saturating_sub(1));
                }
            }
            0x08 => { // backspace
                if g.cur_col > 0 { g.cur_col -= 1; }
            }
            b'\t' => {
                g.cur_col = ((g.cur_col / 8) + 1) * 8;
                if g.cur_col >= g.cols { g.cur_col = g.cols.saturating_sub(1); }
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
            'B' => { let n = p0.max(1); g.cur_row = (g.cur_row + n).min(g.rows.saturating_sub(1)); }
            'C' => { let n = p0.max(1); g.cur_col = (g.cur_col + n).min(g.cols.saturating_sub(1)); }
            'D' => { let n = p0.max(1); g.cur_col = g.cur_col.saturating_sub(n); }
            // Cursor position
            'H' | 'f' => {
                g.cur_row = p0.saturating_sub(1).min(g.rows.saturating_sub(1));
                g.cur_col = p1.saturating_sub(1).min(g.cols.saturating_sub(1));
            }
            // Erase display
            'J' => match p0 {
                0 => { // clear from cursor to end of screen
                    let start = g.flat(g.cur_row, g.cur_col);
                    for c in &mut g.cells[start..] { *c = Cell::default(); }
                }
                1 => { // clear from start to cursor (inclusive)
                    let end = g.flat(g.cur_row, g.cur_col + 1);
                    for c in &mut g.cells[..end] { *c = Cell::default(); }
                }
                _ => { // 2 or 3: clear entire screen
                    for c in g.cells.iter_mut() { *c = Cell::default(); }
                    g.cur_row = 0; g.cur_col = 0;
                }
            }
            // Erase line
            'K' => match p0 {
                0 => { // clear from cursor to end of line
                    let start = g.flat(g.cur_row, g.cur_col);
                    let end   = g.flat(g.cur_row, g.cols);
                    for c in &mut g.cells[start..end] { *c = Cell::default(); }
                }
                1 => { // clear from start of line to cursor (inclusive)
                    let start = g.flat(g.cur_row, 0);
                    let end   = g.flat(g.cur_row, g.cur_col + 1);
                    for c in &mut g.cells[start..end] { *c = Cell::default(); }
                }
                _ => { // 2: clear entire line
                    let start = g.flat(g.cur_row, 0);
                    let end   = g.flat(g.cur_row, g.cols);
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
                            1    => g.application_cursor = enable,
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
            'G' => { g.cur_col = p0.saturating_sub(1).min(g.cols.saturating_sub(1)); }
            'd' => { g.cur_row = p0.saturating_sub(1).min(g.rows.saturating_sub(1)); }
            // Scroll up / down N lines within scroll region
            'S' => { let n = p0.max(1); for _ in 0..n { g.scroll_up(); } }
            'T' => { let n = p0.max(1); for _ in 0..n { g.scroll_down(); } }
            // DECSTBM — set scrolling region
            'r' => {
                let top = p0.saturating_sub(1).min(g.rows.saturating_sub(1));
                let bot = if p1 == 0 { g.rows.saturating_sub(1) } else { (p1 - 1).min(g.rows.saturating_sub(1)) };
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
        // OSC 7: shell reports CWD as file://hostname/path
        if params.len() >= 2 && params[0] == b"7" {
            if let Ok(url) = std::str::from_utf8(params[1]) {
                if let Some(path) = parse_osc7_path(url) {
                    self.grid.cwd = Some(path);
                }
            }
        }
        // OSC 0/2: window/tab title (ignored for now)
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

// Parse OSC 7 "file://hostname/path" or "file:///path" → PathBuf
fn parse_osc7_path(url: &str) -> Option<std::path::PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // file:///path/to/dir — empty hostname
    let path_str = if rest.starts_with('/') {
        rest
    } else {
        // file://hostname/path — skip up to first '/'
        { let i = rest.find('/')?; &rest[i..] }
    };
    // Percent-decode basic %XX sequences
    let decoded = percent_decode(path_str);
    // Reject control bytes (including NUL): any terminal output can emit OSC 7, and a
    // smuggled `%00`/`%1b` would put a NUL or escape into the cwd PathBuf — invalid for
    // any later FFI/display use.
    if decoded.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(std::path::PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (from_hex(bytes[i+1]), from_hex(bytes[i+2])) {
                out.push(char::from(h << 4 | l));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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
    child_pid:   libc::pid_t,
    child_alive: Arc<AtomicBool>,
    closed:      Arc<AtomicBool>,
    pub _reader: std::thread::JoinHandle<()>,
    pub title:   String,
    pub shell:   String,
}

impl Drop for TermPane {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        if self.child_pid > 0 && self.child_alive.load(Ordering::Acquire) {
            // Ask the whole terminal session to exit, then fall back to the
            // shell process itself in case it is not the process-group leader.
            unsafe {
                let _ = libc::kill(-self.child_pid, libc::SIGHUP);
                let _ = libc::kill(self.child_pid, libc::SIGHUP);
            }
        }
        if self.pty_fd >= 0 {
            // SAFETY: pty_fd is a valid open file descriptor owned by this TermPane.
            unsafe { libc::close(self.pty_fd); }
            self.pty_fd = -1;
        }
    }
}

impl TermPane {
    pub fn child_pid(&self) -> libc::pid_t { self.child_pid }
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
    proxy: EventLoopProxy<UserEvent>, cwd: Option<std::path::PathBuf>) -> TermPane {
    spawn_terminal_with_shell(pane_id, cols, rows, proxy, None, cwd)
}

#[cfg(unix)]
pub fn spawn_terminal_with_shell(pane_id: usize, cols: usize, rows: usize,
    proxy: EventLoopProxy<UserEvent>, shell_override: Option<String>,
    cwd: Option<std::path::PathBuf>) -> TermPane {
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
            // Change to the requested working directory before exec (best effort).
            if let Some(ref dir) = cwd {
                use std::os::unix::ffi::OsStrExt;
                if let Ok(c_dir) = CString::new(dir.as_os_str().as_bytes()) {
                    libc::chdir(c_dir.as_ptr()); // ignore failure — shell will report it
                }
            }
            libc::setenv(
                b"TERM\0".as_ptr().cast(),
                b"xterm-256color\0".as_ptr().cast(),
                1,
            );
            // NOTE: we are post-fork in the child. A panic here would unwind through the
            // parent's atexit handlers, so on any error we must fall through to exit(1)
            // rather than unwrap. An embedded NUL in `shell` makes CString::new fail.
            if is_override {
                // The override is a full command string with arguments (e.g.
                // "ssh -o ControlPath=... user@host"). execvp does NOT perform
                // shell word-splitting — it would try to find a file literally
                // named the whole string, which doesn't exist. Delegate to
                // `sh -c <cmd>` so the shell parses the arguments correctly.
                if let (Ok(sh_c), Ok(flag_c), Ok(cmd_c)) = (
                    CString::new("sh"),
                    CString::new("-c"),
                    CString::new(shell.as_str()),
                ) {
                    libc::execvp(
                        sh_c.as_ptr(),
                        [sh_c.as_ptr(), flag_c.as_ptr(), cmd_c.as_ptr(), ptr::null()].as_ptr(),
                    );
                }
            } else if let Ok(shell_c) = CString::new(shell.as_str()) {
                // Plain shell path (e.g. "/bin/zsh") — exec directly with no extra args.
                libc::execvp(shell_c.as_ptr(), [shell_c.as_ptr(), ptr::null()].as_ptr());
            }
            libc::exit(1);
        }
    }

    // Parent: spawn reader thread
    let closed = Arc::new(AtomicBool::new(false));
    let child_alive = Arc::new(AtomicBool::new(true));
    let reader_closed = Arc::clone(&closed);
    let reader_child_alive = Arc::clone(&child_alive);
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            if reader_closed.load(Ordering::Acquire) { break; }
            // SAFETY: master_fd is a valid open PTY master fd; buf is a mutable
            // stack-allocated slice sized to match the length argument.
            let n = unsafe { libc::read(master_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) { continue; }
                let _ = proxy.send_event(UserEvent::Redraw);
                break;
            }
            if n <= 0 {
                let _ = proxy.send_event(UserEvent::Redraw);
                break;
            }
            if reader_closed.load(Ordering::Acquire) { break; }
            // Box<[u8]> carries exactly n bytes with no excess capacity.
            let data: Box<[u8]> = buf[..n as usize].into();
            if proxy.send_event(UserEvent::TermOutput { pane_id, data }).is_err() { break; }
        }
        if pid > 0 {
            let mut status: libc::c_int = 0;
            // Reap the forked shell/ssh process off the UI thread.
            unsafe { let _ = libc::waitpid(pid, &mut status, 0); }
            reader_child_alive.store(false, Ordering::Release);
        }
    });

    TermPane {
        id: pane_id,
        grid: TermGrid::new(cols, rows),
        parser: Parser::new(),
        pty_fd: master_fd,
        child_pid: pid,
        child_alive,
        closed,
        _reader: reader,
        title: "Terminal".to_owned(),
        shell,
    }
}

/// Create a display-only TermPane with no PTY and no child process.
/// Used to show LSP server output (stderr/stdout) with full VT100 rendering.
/// `pty_fd = -1` means writes are silently discarded; Drop guards are already
/// conditioned on `child_pid > 0` and `pty_fd >= 0`.
#[cfg(unix)]
pub fn new_log_pane(id: usize, cols: usize, rows: usize, title: String) -> TermPane {
    let child_alive = Arc::new(AtomicBool::new(false));
    let closed      = Arc::new(AtomicBool::new(false));
    TermPane {
        id,
        grid:        TermGrid::new(cols, rows),
        parser:      Parser::new(),
        pty_fd:      -1,
        child_pid:   -1,
        child_alive,
        closed,
        _reader:     std::thread::spawn(|| {}),
        title,
        shell:       String::new(),
    }
}

// ── Key encoding ──────────────────────────────────────────────────────────────

/// Encode a winit keyboard event into bytes to write to the PTY master fd.
/// Returns None for events that should not be forwarded.
pub fn encode_key(
    key: &Key,
    mods: ModifiersState,
    text: Option<&str>,
    application_cursor: bool,
) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let cursor_key = |suffix| {
        vec![0x1b, if application_cursor { b'O' } else { b'[' }, suffix]
    };
    match key {
        Key::Named(NamedKey::Enter)      => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace)  => Some(b"\x7f".to_vec()),
        Key::Named(NamedKey::Escape)     => Some(b"\x1b".to_vec()),
        Key::Named(NamedKey::Tab)        => {
            if mods.shift_key() { Some(b"\x1b[Z".to_vec()) } else { Some(b"\t".to_vec()) }
        }
        Key::Named(NamedKey::ArrowUp)    => Some(cursor_key(b'A')),
        Key::Named(NamedKey::ArrowDown)  => Some(cursor_key(b'B')),
        Key::Named(NamedKey::ArrowRight) => Some(cursor_key(b'C')),
        Key::Named(NamedKey::ArrowLeft)  => Some(cursor_key(b'D')),
        Key::Named(NamedKey::Home)       => Some(cursor_key(b'H')),
        Key::Named(NamedKey::End)        => Some(cursor_key(b'F')),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive raw bytes straight into a grid (no PTY) for parser-level tests.
    fn drive(grid: &mut TermGrid, bytes: &[u8]) {
        let mut parser = Parser::new();
        let mut handler = VteHandler { grid };
        for &b in bytes {
            parser.advance(&mut handler, b);
        }
    }

    #[test]
    fn erase_at_deferred_wrap_does_not_panic() {
        // Fill exactly cols*rows cells so the cursor parks one past the last column
        // (deferred wrap), then issue every erase variant. CSI 1J / 1K used to build
        // inclusive ranges that indexed cells[..=len] and panicked.
        for seq in [
            b"\x1b[J".as_ref(), b"\x1b[1J", b"\x1b[2J",
            b"\x1b[K", b"\x1b[1K", b"\x1b[2K",
        ] {
            let mut g = TermGrid::new(4, 3);
            drive(&mut g, b"abcdefghijkl"); // 12 chars == 4*3
            drive(&mut g, seq);             // must not panic
        }
    }

    #[test]
    fn zero_sized_grid_survives_output() {
        // A degenerate 0x0 grid must not panic on prints, cursor moves, erases, tabs.
        let mut g = TermGrid::new(0, 0);
        drive(&mut g, b"hello\x1b[2J\x1b[5;5H\x1b[1K\tworld\n\x1b[10A\x1b[10C");
    }

    #[test]
    fn osc7_rejects_control_bytes() {
        assert!(parse_osc7_path("file:///home/user/%00etc").is_none());
        assert!(parse_osc7_path("file:///home/%1bevil").is_none());
        assert_eq!(
            parse_osc7_path("file:///home/user/proj"),
            Some(std::path::PathBuf::from("/home/user/proj")),
        );
    }

    #[test]
    fn application_cursor_mode_changes_arrow_sequences() {
        let mut g = TermGrid::new(8, 2);
        drive(&mut g, b"\x1b[?1h");
        assert!(g.application_cursor);
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::ArrowUp),
                ModifiersState::empty(),
                None,
                g.application_cursor,
            ),
            Some(b"\x1bOA".to_vec()),
        );
        drive(&mut g, b"\x1b[?1l");
        assert!(!g.application_cursor);
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::ArrowLeft),
                ModifiersState::empty(),
                None,
                g.application_cursor,
            ),
            Some(b"\x1b[D".to_vec()),
        );
    }

    #[test]
    fn resize_keeps_displaced_rows_in_scrollback() {
        let mut g = TermGrid::new(4, 3);
        drive(&mut g, b"one\r\ntwo\r\ntri");
        g.resize(4, 2);

        let row_text = |row: &[Cell]| row.iter().map(|cell| cell.ch).collect::<String>();
        assert_eq!(row_text(g.scrollback.back().unwrap()).trim_end(), "one");
        assert_eq!(row_text(&g.cells[..4]).trim_end(), "two");
        assert_eq!(row_text(&g.cells[4..8]).trim_end(), "tri");
    }

    #[test]
    fn resize_preserves_saved_primary_screen_during_alt_screen() {
        let mut g = TermGrid::new(6, 2);
        drive(&mut g, b"shell");
        drive(&mut g, b"\x1b[?1049hfull");
        g.resize(4, 2);
        assert!(g.alt_cells.is_some());
        drive(&mut g, b"\x1b[?1049l");
        let restored: String = g.cells.iter().map(|cell| cell.ch).collect();
        assert!(restored.contains("shel"));
    }
}
