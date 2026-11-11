// Text layout helpers: tab stops, visual columns and soft word wrap.
//
// The editor stores positions as char offsets. On screen a tab advances to the next
// tab stop and, with word wrap on, a logical line is split into several visual rows.
// Everything that converts between "char column" and "pixel x / screen row" goes
// through these functions so rendering, hit-testing and cursor movement agree.
//
// Within a row, tab stops are measured from the start of that row (a continuation
// row starts at visual column 0).

use ropey::Rope;

/// How text is laid out in one editor view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    /// Width of a tab stop in cells (>= 1).
    pub tab_w: usize,
    /// Soft-wrap width in cells, or None for no wrapping.
    pub wrap:  Option<usize>,
}

impl Layout {
    pub fn new(tab_w: usize, wrap: Option<usize>) -> Self {
        Layout { tab_w: tab_w.max(1), wrap: wrap.map(|w| w.max(8)) }
    }
}

/// Cells taken by `ch` when it starts at visual column `v`.
#[inline]
pub fn char_cells(ch: char, v: usize, tab_w: usize) -> usize {
    if ch == '\t' { tab_w - v % tab_w } else { 1 }
}

/// Visual column of every char boundary of `chars` (length `chars.len() + 1`).
pub fn vcols(chars: &[char], tab_w: usize) -> Vec<u32> {
    let mut out = Vec::with_capacity(chars.len() + 1);
    let mut v = 0usize;
    out.push(0);
    for &ch in chars {
        v += char_cells(ch, v, tab_w);
        out.push(v as u32);
    }
    out
}

/// Visual column of char column `col` (clamped) within `chars`.
pub fn vcol_of(chars: &[char], col: usize, tab_w: usize) -> usize {
    let mut v = 0;
    for &ch in chars.iter().take(col) { v += char_cells(ch, v, tab_w); }
    v
}

/// Char column whose cell contains visual column `vx` (nearest boundary; rounds a
/// click on the right half of a wide tab to after it).
pub fn col_at_vcol(chars: &[char], vx: usize, tab_w: usize) -> usize {
    let mut v = 0;
    for (i, &ch) in chars.iter().enumerate() {
        let w = char_cells(ch, v, tab_w);
        if vx < v + w {
            return if w > 1 && vx >= v + w.div_ceil(2) { i + 1 } else { i };
        }
        v += w;
    }
    chars.len()
}

/// Start column of every visual row of a line (always starts with 0). Without a
/// wrap width the whole line is one row. Rows break after the last space that fits;
/// a word longer than the width is split.
pub fn wrap_rows(chars: &[char], layout: Layout) -> Vec<usize> {
    let mut rows = vec![0];
    let Some(width) = layout.wrap else { return rows };
    let mut row_start = 0;
    let mut v = 0usize;          // visual col within the current row
    let mut last_break: Option<usize> = None; // col just after a space in this row
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let w = char_cells(ch, v, layout.tab_w);
        if v + w > width && i > row_start {
            let brk = match last_break {
                Some(b) if b > row_start && b <= i => b,
                _ => i,
            };
            rows.push(brk);
            row_start = brk;
            last_break = None;
            // Re-measure from the new row start.
            v = 0;
            i = brk;
            continue;
        }
        v += w;
        i += 1;
        if ch == ' ' || ch == '\t' { last_break = Some(i); }
    }
    rows
}

/// Number of visual rows in `line` of `text`.
pub fn line_row_count(text: &Rope, line: usize, layout: Layout) -> usize {
    if layout.wrap.is_none() { return 1; }
    wrap_rows(&line_chars(text, line), layout).len()
}

/// The chars of `line` without its line ending.
pub fn line_chars(text: &Rope, line: usize) -> Vec<char> {
    if line >= text.len_lines() { return Vec::new(); }
    text.line(line).chars().take_while(|&c| c != '\n' && c != '\r').collect()
}

/// Which row of `rows` (from `wrap_rows`) holds char column `col`. A column on a
/// row boundary belongs to the following row (the cursor is drawn at its start).
pub fn row_of_col(rows: &[usize], col: usize) -> usize {
    rows.partition_point(|&start| start <= col).saturating_sub(1)
}

/// Move `delta` visual rows from (line, sub-row), clamped to the document.
pub fn advance_rows(text: &Rope, line: usize, sub: usize, mut delta: isize, layout: Layout, last_line: usize) -> (usize, usize) {
    let mut line = line.min(last_line);
    let mut sub = sub.min(line_row_count(text, line, layout).saturating_sub(1));
    while delta > 0 {
        let n = line_row_count(text, line, layout);
        if sub + 1 < n { sub += 1; }
        else if line < last_line { line += 1; sub = 0; }
        else { break; }
        delta -= 1;
    }
    while delta < 0 {
        if sub > 0 { sub -= 1; }
        else if line > 0 {
            line -= 1;
            sub = line_row_count(text, line, layout).saturating_sub(1);
        } else { break; }
        delta += 1;
    }
    (line, sub)
}

/// Visual rows from (l0, s0) forward to (l1, s1), counting at most `cap`.
/// Returns None if (l1, s1) is before (l0, s0).
pub fn rows_between(text: &Rope, l0: usize, s0: usize, l1: usize, s1: usize, layout: Layout, cap: usize) -> Option<usize> {
    if (l1, s1) < (l0, s0) { return None; }
    if layout.wrap.is_none() { return Some((l1 - l0).min(cap)); }
    if l0 == l1 { return Some(s1 - s0); }
    let mut n = line_row_count(text, l0, layout).saturating_sub(s0);
    let mut l = l0 + 1;
    while l < l1 && n < cap {
        n += line_row_count(text, l, layout);
        l += 1;
    }
    Some((n + s1).min(cap))
}

// ── Indentation detection ─────────────────────────────────────────────────────

/// Indentation style of a buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Indent {
    pub use_tabs: bool,
    /// Tab stop width; also the number of spaces per level when !use_tabs.
    pub size:     usize,
}

impl Indent {
    /// The text one indent level inserts.
    pub fn unit(self) -> String {
        if self.use_tabs { "\t".to_owned() } else { " ".repeat(self.size.max(1)) }
    }
    pub fn label(self) -> String {
        if self.use_tabs { format!("Tab Size: {}", self.size) } else { format!("Spaces: {}", self.size) }
    }
}

/// Process-wide defaults for new buffers (mirrors the settings, so code that makes a
/// tab without the settings at hand still gets the user's preference):
/// bits 0..8 = size, bit 8 = use tabs, bit 9 = detect from content.
static DEFAULTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(4 | 1 << 9);

pub fn set_defaults(indent: Indent, detect: bool) {
    let v = indent.size.clamp(1, 255) | (indent.use_tabs as usize) << 8 | (detect as usize) << 9;
    DEFAULTS.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn default_indent() -> Indent {
    let v = DEFAULTS.load(std::sync::atomic::Ordering::Relaxed);
    Indent { use_tabs: v & 1 << 8 != 0, size: (v & 0xFF).max(1) }
}

/// The default indentation, refined by `detect_indent` when detection is enabled.
pub fn indent_for(text: &Rope) -> Indent {
    let base = default_indent();
    let detect = DEFAULTS.load(std::sync::atomic::Ordering::Relaxed) & 1 << 9 != 0;
    match detect.then(|| detect_indent(text)).flatten() {
        Some(Indent { use_tabs: true, .. }) => Indent { use_tabs: true, size: base.size },
        Some(found) => found,
        None => base,
    }
}

/// Guess the indentation of `text` from its first few thousand lines: tabs if more
/// lines are indented with tabs than with spaces, otherwise the most common change in
/// leading spaces between consecutive lines (2, 4 or 8, else 4). Returns None when the
/// text has (almost) no indented lines, so the caller keeps its default.
pub fn detect_indent(text: &Rope) -> Option<Indent> {
    let mut tab_lines = 0usize;
    let mut space_lines = 0usize;
    let mut deltas = [0usize; 9];
    let mut prev_spaces = 0usize;
    for line in text.lines().take(5000) {
        let mut spaces = 0usize;
        let mut first = None;
        for ch in line.chars() {
            match ch {
                ' ' => spaces += 1,
                _ => { first = Some(ch); break; }
            }
        }
        let Some(first) = first else { continue }; // blank / whitespace-only
        if first == '\n' || first == '\r' { continue; }
        if first == '\t' && spaces == 0 {
            tab_lines += 1;
            continue;
        }
        // Skip lines that continue a block comment (" * foo") — they're off by one.
        if first == '*' { continue; }
        if spaces > 0 { space_lines += 1; }
        let d = spaces.abs_diff(prev_spaces);
        if (2..=8).contains(&d) { deltas[d] += 1; }
        prev_spaces = spaces;
    }
    if tab_lines + space_lines < 2 { return None; }
    if tab_lines > space_lines {
        return Some(Indent { use_tabs: true, size: 0 });
    }
    // Prefer the most frequent indent step; ties go to the smaller common width.
    let best = [2usize, 4, 8, 3, 6].into_iter()
        .max_by_key(|&d| (deltas[d], std::cmp::Reverse(d)))
        .filter(|&d| deltas[d] > 0)
        .unwrap_or(4);
    Some(Indent { use_tabs: false, size: best })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(s: &str) -> Vec<char> { s.chars().collect() }

    #[test]
    fn tabs_advance_to_the_next_stop() {
        assert_eq!(vcols(&cs("a\tb"), 4), vec![0, 1, 4, 5]);
        assert_eq!(vcols(&cs("\t\t"), 4), vec![0, 4, 8]);
        assert_eq!(vcol_of(&cs("ab\tc"), 3, 4), 4);
        assert_eq!(col_at_vcol(&cs("a\tb"), 2, 4), 1);  // left half of the tab
        assert_eq!(col_at_vcol(&cs("a\tb"), 3, 4), 2);  // right half
        assert_eq!(col_at_vcol(&cs("a\tb"), 99, 4), 3); // past the end
    }

    #[test]
    fn wrap_prefers_spaces_and_splits_long_words() {
        let l = Layout::new(4, Some(10));
        assert_eq!(wrap_rows(&cs("hello world again"), l), vec![0, 6, 12]);
        assert_eq!(wrap_rows(&cs("hi you there"), l), vec![0, 7]);
        assert_eq!(wrap_rows(&cs("abcdefghijklmnopqrstuvwxy"), l), vec![0, 10, 20]);
        assert_eq!(wrap_rows(&cs("short"), l), vec![0]);
        assert_eq!(wrap_rows(&cs("short"), Layout::new(4, None)), vec![0]);
    }

    #[test]
    fn boundary_columns_belong_to_the_next_row() {
        let rows = [0, 12];
        assert_eq!(row_of_col(&rows, 11), 0);
        assert_eq!(row_of_col(&rows, 12), 1);
        assert_eq!(row_of_col(&rows, 20), 1);
    }

    #[test]
    fn advance_and_count_rows_across_wrapped_lines() {
        let text = Rope::from_str("aaaa aaaa aaaa\nb\ncccc cccc cccc\n");
        let l = Layout::new(4, Some(10));
        let last = 2;
        assert_eq!(line_row_count(&text, 0, l), 2);
        assert_eq!(advance_rows(&text, 0, 0, 1, l, last), (0, 1));
        assert_eq!(advance_rows(&text, 0, 0, 2, l, last), (1, 0));
        assert_eq!(advance_rows(&text, 0, 0, 99, l, last), (2, 1));
        assert_eq!(advance_rows(&text, 2, 1, -3, l, last), (0, 1));
        assert_eq!(rows_between(&text, 0, 0, 2, 1, l, 100), Some(4));
        assert_eq!(rows_between(&text, 0, 1, 0, 1, l, 100), Some(0));
        assert_eq!(rows_between(&text, 1, 0, 0, 1, l, 100), None);
    }

    #[test]
    fn detects_common_indent_styles() {
        let two = Rope::from_str("a {\n  b {\n    c\n  }\n}\n");
        assert_eq!(detect_indent(&two), Some(Indent { use_tabs: false, size: 2 }));
        let four = Rope::from_str("def f():\n    if x:\n        y\n    return\n");
        assert_eq!(detect_indent(&four), Some(Indent { use_tabs: false, size: 4 }));
        let tabs = Rope::from_str("func f() {\n\tif x {\n\t\ty()\n\t}\n}\n");
        assert_eq!(detect_indent(&tabs).map(|i| i.use_tabs), Some(true));
        assert_eq!(detect_indent(&Rope::from_str("no\nindent\n")), None);
    }
}
