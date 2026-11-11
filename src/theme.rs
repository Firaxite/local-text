// Color themes.
//
// All UI and syntax colors come from the current `Theme` (0x00RRGGBB). The renderer
// reads it on every frame, so switching themes recolors everything at once; cached
// syntax colors are invalidated by the caller. Terminal cells store "default fg/bg"
// and the 16 ANSI colors as sentinels (see `terminal`) resolved here at draw time.

use std::sync::atomic::{AtomicPtr, Ordering};

#[derive(Clone, Debug)]
pub struct Theme {
    pub name:            &'static str,
    pub bg:              u32,
    pub bg2:             u32,
    pub fg:              u32,
    pub fg_dim:          u32,
    pub accent:          u32,
    pub border:          u32,
    pub sel_bg:          u32,
    pub hl_keyword:      u32,
    pub hl_string:       u32,
    pub hl_number:       u32,
    pub hl_comment:      u32,
    pub hl_type:         u32,
    pub hl_func:         u32,
    pub hl_match:        u32,
    pub hl_match_active: u32,
    pub diff_add_fg:     u32,
    pub diff_del_fg:     u32,
    pub diff_add_bg:     u32,
    pub diff_del_bg:     u32,
    pub diff_hunk:       u32,
    pub sb_thumb:        u32,
    pub drag_zone:       u32,
    pub error:           u32,
    pub warning:         u32,
    /// Status-bar messages.
    pub notice:          u32,
    pub ok:              u32,
    pub rainbow:         [u32; 6],
    /// Terminal default foreground (the background is `bg`).
    pub term_fg:         u32,
    pub ansi:            [u32; 16],
}

pub const TOKYO_NIGHT: Theme = Theme {
    name: "Tokyo Night",
    bg: 0x1A1B26, bg2: 0x24283B, fg: 0xA9B1D6, fg_dim: 0x565F89, accent: 0x7AA2F7,
    border: 0x3B4261, sel_bg: 0x2D3149,
    hl_keyword: 0xBB9AF7, hl_string: 0x9ECE6A, hl_number: 0xFF9E64, hl_comment: 0x565F89,
    hl_type: 0x2AC3DE, hl_func: 0x7AA2F7,
    hl_match: 0x3D3557, hl_match_active: 0x524175,
    diff_add_fg: 0x9ECE6A, diff_del_fg: 0xF7768E, diff_add_bg: 0x20322E, diff_del_bg: 0x32201E,
    diff_hunk: 0x7DCFFF,
    sb_thumb: 0x414868, drag_zone: 0x2A3F6F,
    error: 0xFF5555, warning: 0xE0AF68, notice: 0xFFB86C, ok: 0x6BCB77,
    rainbow: [0xFF79C6, 0xFFB86C, 0xF1FA8C, 0x50FA7B, 0x8BE9FD, 0xBD93F9],
    term_fg: 0xA9B1D6,
    ansi: [
        0x15161E, 0xF7768E, 0x9ECE6A, 0xE0AF68, 0x7AA2F7, 0xBB9AF7, 0x7DCFFF, 0xA9B1D6,
        0x414868, 0xF7768E, 0x9ECE6A, 0xE0AF68, 0x7AA2F7, 0xBB9AF7, 0x7DCFFF, 0xC0CAF5,
    ],
};

pub const MIDNIGHT: Theme = Theme {
    name: "Midnight",
    bg: 0x111317, bg2: 0x1A1D23, fg: 0xC8CCD4, fg_dim: 0x5C6370, accent: 0x61AFEF,
    border: 0x2C313A, sel_bg: 0x283040,
    hl_keyword: 0xC678DD, hl_string: 0x98C379, hl_number: 0xD19A66, hl_comment: 0x5C6370,
    hl_type: 0xE5C07B, hl_func: 0x61AFEF,
    hl_match: 0x3A3F4B, hl_match_active: 0x51586A,
    diff_add_fg: 0x98C379, diff_del_fg: 0xE06C75, diff_add_bg: 0x1B2B1F, diff_del_bg: 0x2E1C1E,
    diff_hunk: 0x56B6C2,
    sb_thumb: 0x3A3F4B, drag_zone: 0x22344F,
    error: 0xE06C75, warning: 0xE5C07B, notice: 0xD19A66, ok: 0x98C379,
    rainbow: [0xE06C75, 0xD19A66, 0xE5C07B, 0x98C379, 0x56B6C2, 0xC678DD],
    term_fg: 0xC8CCD4,
    ansi: [
        0x1A1D23, 0xE06C75, 0x98C379, 0xE5C07B, 0x61AFEF, 0xC678DD, 0x56B6C2, 0xC8CCD4,
        0x5C6370, 0xEF7B84, 0xA9D18A, 0xF0CD8E, 0x7CBFF5, 0xD38BE6, 0x6CC6D2, 0xFFFFFF,
    ],
};

pub const PAPER_LIGHT: Theme = Theme {
    name: "Paper Light",
    bg: 0xFAFAF8, bg2: 0xEDEDEA, fg: 0x2E3440, fg_dim: 0x8A8F98, accent: 0x2F6FDB,
    border: 0xD0D0CC, sel_bg: 0xCFE0FA,
    hl_keyword: 0x8B3FBF, hl_string: 0x2E7D32, hl_number: 0xB45309, hl_comment: 0x8A8F98,
    hl_type: 0x00799E, hl_func: 0x2F6FDB,
    hl_match: 0xF6E7A9, hl_match_active: 0xF1CF5A,
    diff_add_fg: 0x1E6B2B, diff_del_fg: 0xB3261E, diff_add_bg: 0xE3F4E5, diff_del_bg: 0xFBE5E3,
    diff_hunk: 0x00799E,
    sb_thumb: 0xC4C4C0, drag_zone: 0xC9DBF7,
    error: 0xC62828, warning: 0xA86400, notice: 0xB45309, ok: 0x2E7D32,
    rainbow: [0xC2185B, 0xB45309, 0x8D6E00, 0x2E7D32, 0x00799E, 0x6A3FBF],
    term_fg: 0x2E3440,
    ansi: [
        0x2E3440, 0xC62828, 0x2E7D32, 0x8D6E00, 0x2F6FDB, 0x8B3FBF, 0x00799E, 0x8A8F98,
        0x5C6370, 0xE53935, 0x43A047, 0xB28704, 0x4C8BF5, 0xA45BD6, 0x0097A7, 0xBDBDBD,
    ],
};

pub const HIGH_CONTRAST: Theme = Theme {
    name: "High Contrast",
    bg: 0x000000, bg2: 0x0C0C0C, fg: 0xFFFFFF, fg_dim: 0xB0B0B0, accent: 0x4FC1FF,
    border: 0x6E6E6E, sel_bg: 0x264F78,
    hl_keyword: 0xFF9CFF, hl_string: 0x9CFF9C, hl_number: 0xFFD27F, hl_comment: 0x8FBF8F,
    hl_type: 0x7FE7FF, hl_func: 0xFFFF7F,
    hl_match: 0x5A4A00, hl_match_active: 0x8A6E00,
    diff_add_fg: 0x9CFF9C, diff_del_fg: 0xFF8080, diff_add_bg: 0x003300, diff_del_bg: 0x400000,
    diff_hunk: 0x7FE7FF,
    sb_thumb: 0x8A8A8A, drag_zone: 0x003A66,
    error: 0xFF6060, warning: 0xFFD27F, notice: 0xFFD27F, ok: 0x9CFF9C,
    rainbow: [0xFF9CFF, 0xFFD27F, 0xFFFF7F, 0x9CFF9C, 0x7FE7FF, 0xC6A0FF],
    term_fg: 0xFFFFFF,
    ansi: [
        0x000000, 0xFF6060, 0x9CFF9C, 0xFFFF7F, 0x7FB2FF, 0xFF9CFF, 0x7FE7FF, 0xE0E0E0,
        0x808080, 0xFF8080, 0xB8FFB8, 0xFFFFA8, 0xA8C8FF, 0xFFB8FF, 0xA8F0FF, 0xFFFFFF,
    ],
};

pub const BUILTIN: [&Theme; 4] = [&TOKYO_NIGHT, &MIDNIGHT, &PAPER_LIGHT, &HIGH_CONTRAST];

static CURRENT: AtomicPtr<Theme> = AtomicPtr::new(std::ptr::null_mut());

/// The active theme.
#[inline]
pub fn current() -> &'static Theme {
    let p = CURRENT.load(Ordering::Acquire);
    if p.is_null() { &TOKYO_NIGHT } else {
        // SAFETY: only ever set from `Box::leak` (never freed), so the pointer stays valid.
        unsafe { &*p }
    }
}

/// Activate the built-in theme `name` (case-insensitive; unknown → the default)
/// with `overrides` (field name → "#RRGGBB") applied on top. Returns false if the
/// name or an override was not recognised.
pub fn apply(name: &str, overrides: &std::collections::HashMap<String, String>) -> bool {
    let base = BUILTIN.iter().find(|t| t.name.eq_ignore_ascii_case(name));
    let mut ok = base.is_some();
    let mut t = (*base.copied().unwrap_or(&TOKYO_NIGHT)).clone();
    for (key, value) in overrides {
        let Some(color) = parse_hex(value) else { ok = false; continue };
        if !set_field(&mut t, key, color) { ok = false; }
    }
    // Leaked on purpose: themes change rarely and readers hold `&'static` references.
    CURRENT.store(Box::leak(Box::new(t)), Ordering::Release);
    ok
}

fn parse_hex(v: &str) -> Option<u32> {
    let h = v.trim().trim_start_matches('#');
    (h.len() == 6).then(|| u32::from_str_radix(h, 16).ok()).flatten()
}

fn set_field(t: &mut Theme, key: &str, c: u32) -> bool {
    let slot = match key {
        "bg" => &mut t.bg, "bg2" => &mut t.bg2, "fg" => &mut t.fg, "fg_dim" => &mut t.fg_dim,
        "accent" => &mut t.accent, "border" => &mut t.border, "sel_bg" => &mut t.sel_bg,
        "hl_keyword" => &mut t.hl_keyword, "hl_string" => &mut t.hl_string,
        "hl_number" => &mut t.hl_number, "hl_comment" => &mut t.hl_comment,
        "hl_type" => &mut t.hl_type, "hl_func" => &mut t.hl_func,
        "hl_match" => &mut t.hl_match, "hl_match_active" => &mut t.hl_match_active,
        "diff_add_fg" => &mut t.diff_add_fg, "diff_del_fg" => &mut t.diff_del_fg,
        "diff_add_bg" => &mut t.diff_add_bg, "diff_del_bg" => &mut t.diff_del_bg,
        "diff_hunk" => &mut t.diff_hunk, "sb_thumb" => &mut t.sb_thumb,
        "drag_zone" => &mut t.drag_zone, "error" => &mut t.error, "warning" => &mut t.warning,
        "notice" => &mut t.notice, "ok" => &mut t.ok, "term_fg" => &mut t.term_fg,
        _ => {
            if let Some(i) = key.strip_prefix("ansi").and_then(|n| n.parse::<usize>().ok()).filter(|&i| i < 16) {
                &mut t.ansi[i]
            } else if let Some(i) = key.strip_prefix("rainbow").and_then(|n| n.parse::<usize>().ok()).filter(|&i| i < 6) {
                &mut t.rainbow[i]
            } else {
                return false;
            }
        }
    };
    *slot = c;
    true
}

/// Resolve a terminal cell color (sentinels from `terminal`) for drawing.
#[inline]
pub fn term_color(c: u32) -> u32 {
    let t = current();
    match c {
        crate::terminal::DEFAULT_FG => t.term_fg,
        crate::terminal::DEFAULT_BG => t.bg,
        _ if c & crate::terminal::ANSI_FLAG != 0 => t.ansi[(c & 0xF) as usize],
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_apply_on_top_of_a_builtin() {
        let mut o = std::collections::HashMap::new();
        o.insert("accent".to_owned(), "#123456".to_owned());
        o.insert("ansi3".to_owned(), "abcdef".to_owned());
        assert!(apply("paper light", &o));
        assert_eq!(current().name, "Paper Light");
        assert_eq!(current().accent, 0x123456);
        assert_eq!(current().ansi[3], 0xABCDEF);
        o.insert("nope".to_owned(), "#000000".to_owned());
        assert!(!apply("Paper Light", &o));
        assert!(!apply("no such theme", &Default::default()));
        assert_eq!(current().name, "Tokyo Night");
        apply("Tokyo Night", &Default::default());
    }
}
