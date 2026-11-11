// Regular-expression search and replace for the find bar and global search.
//
// Patterns match line by line (so `^`/`$` and `.` behave per line, and CRLF endings
// are never part of a match) unless the pattern mentions a line break (`\n`), in
// which case the whole document is searched. Results are char offsets, like every
// other position in the editor. Empty matches are skipped: they can't be shown or
// replaced meaningfully.

use regex::{Regex, RegexBuilder};
use ropey::Rope;

/// Upper bound on matches collected from one document (keeps a pathological pattern
/// like `.` on a huge file from allocating without limit).
const MAX_MATCHES: usize = 200_000;

/// Compile a find-bar pattern. `whole_word` wraps it in word boundaries.
pub fn build(pattern: &str, case_sensitive: bool, whole_word: bool) -> Result<Regex, String> {
    let pat = if whole_word { format!(r"\b(?:{pattern})\b") } else { pattern.to_owned() };
    RegexBuilder::new(&pat)
        .case_insensitive(!case_sensitive)
        .multi_line(true)
        .size_limit(16 << 20)
        .build()
        .map_err(|e| match e {
            regex::Error::Syntax(msg) => msg.lines().last().unwrap_or("invalid pattern").trim().to_owned(),
            other => other.to_string(),
        })
}

/// Does the pattern span lines (and so need the whole document at once)?
pub fn is_multiline(pattern: &str) -> bool {
    pattern.contains("\\n") || pattern.contains('\n')
}

/// Convert sorted byte offsets in `s` to char offsets.
fn bytes_to_chars(s: &str, offsets: &[usize]) -> Vec<usize> {
    let mut out = Vec::with_capacity(offsets.len());
    let mut chars = 0;
    let mut last = 0;
    for &off in offsets {
        chars += s[last..off].chars().count();
        last = off;
        out.push(chars);
    }
    out
}

/// Non-empty matches in `s` as char ranges, plus the replacement for each when
/// `replace` is given.
fn str_matches(re: &Regex, s: &str, replace: Option<&str>, cap: usize) -> Vec<(usize, usize, String)> {
    let mut spans: Vec<(usize, usize, String)> = Vec::new();
    for caps in re.captures_iter(s) {
        let Some(m) = caps.get(0) else { continue };
        if m.start() == m.end() { continue; }
        let mut text = String::new();
        if let Some(r) = replace { caps.expand(r, &mut text); }
        spans.push((m.start(), m.end(), text));
        if spans.len() >= cap { break; }
    }
    let flat: Vec<usize> = spans.iter().flat_map(|&(a, b, _)| [a, b]).collect();
    let chars = bytes_to_chars(s, &flat);
    spans.into_iter().enumerate()
        .map(|(i, (_, _, t))| (chars[2 * i], chars[2 * i + 1], t))
        .collect()
}

/// Matches in a document (char ranges), with replacements when `replace` is given.
fn rope_matches_impl(re: &Regex, text: &Rope, multiline: bool, replace: Option<&str>) -> Vec<(usize, usize, String)> {
    if multiline {
        let whole: String = text.chunks().collect();
        return str_matches(re, &whole, replace, MAX_MATCHES);
    }
    let mut out = Vec::new();
    let mut line_start = 0usize;
    let mut buf = String::new();
    for line in text.lines() {
        buf.clear();
        buf.extend(line.chunks());
        let body = buf.trim_end_matches(['\n', '\r']);
        for (lo, hi, t) in str_matches(re, body, replace, MAX_MATCHES - out.len()) {
            out.push((line_start + lo, line_start + hi, t));
        }
        if out.len() >= MAX_MATCHES { break; }
        line_start += line.len_chars();
    }
    out
}

pub fn rope_matches(re: &Regex, text: &Rope, multiline: bool) -> Vec<(usize, usize)> {
    rope_matches_impl(re, text, multiline, None).into_iter().map(|(a, b, _)| (a, b)).collect()
}

/// Every match with its expanded replacement (`$1`, `${name}`, `$$`).
pub fn rope_replacements(re: &Regex, text: &Rope, multiline: bool, replace: &str) -> Vec<(usize, usize, String)> {
    rope_matches_impl(re, text, multiline, Some(&unescape_replacement(replace)))
}

/// The replacement for the match that exactly covers `lo..hi`, if there is one.
pub fn replacement_for(re: &Regex, text: &Rope, multiline: bool, replace: &str, lo: usize, hi: usize) -> Option<String> {
    rope_replacements(re, text, multiline, replace).into_iter()
        .find(|&(a, b, _)| a == lo && b == hi)
        .map(|(_, _, t)| t)
}

/// Matches within one line (char ranges) — global search.
pub fn line_matches(re: &Regex, line: &str) -> Vec<(usize, usize)> {
    str_matches(re, line, None, 1000).into_iter().map(|(a, b, _)| (a, b)).collect()
}

/// Replace every match in `content` (line by line unless `multiline`), keeping line
/// endings. Returns the new text and the count, or None if nothing matched.
pub fn replace_in_str(re: &Regex, content: &str, multiline: bool, replace: &str) -> Option<(String, usize)> {
    let replace = unescape_replacement(replace);
    if multiline {
        let mut count = 0;
        let out = re.replace_all(content, |caps: &regex::Captures| {
            let m = caps.get(0).map_or("", |m| m.as_str());
            if m.is_empty() { return String::new(); }
            count += 1;
            let mut t = String::new();
            caps.expand(&replace, &mut t);
            t
        });
        return (count > 0).then(|| (out.into_owned(), count));
    }
    let mut out = String::with_capacity(content.len());
    let mut count = 0;
    for line in content.split_inclusive('\n') {
        let (body, ending) = if let Some(b) = line.strip_suffix("\r\n") { (b, "\r\n") }
            else if let Some(b) = line.strip_suffix('\n') { (b, "\n") }
            else { (line, "") };
        let mut last = 0;
        let mut changed = false;
        for caps in re.captures_iter(body) {
            let Some(m) = caps.get(0) else { continue };
            if m.start() == m.end() { continue; }
            out.push_str(&body[last..m.start()]);
            caps.expand(&replace, &mut out);
            last = m.end();
            count += 1;
            changed = true;
        }
        if changed {
            out.push_str(&body[last..]);
            out.push_str(ending);
        } else {
            out.push_str(line);
        }
    }
    (count > 0).then_some((out, count))
}

/// `\n`, `\t` and `\\` in a regex-mode replacement mean a newline, a tab and a
/// backslash (as in other editors); everything else is kept for `Captures::expand`.
fn unescape_replacement(r: &str) -> String {
    let mut out = String::with_capacity(r.len());
    let mut it = r.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.peek() {
                Some('n') => { it.next(); out.push('\n'); }
                Some('t') => { it.next(); out.push('\t'); }
                Some('\\') => { it.next(); out.push('\\'); }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_are_char_offsets_and_skip_empty_matches() {
        let text = Rope::from_str("héllo wörld\nfoo123 bar45\n");
        let re = build(r"\d+", true, false).unwrap();
        assert_eq!(rope_matches(&re, &text, false), vec![(15, 18), (22, 24)]);
        let re = build(r"w.rld", true, false).unwrap();
        assert_eq!(rope_matches(&re, &text, false), vec![(6, 11)]);
        let re = build(r"x*", true, false).unwrap();
        assert!(rope_matches(&re, &text, false).is_empty());
    }

    #[test]
    fn anchors_work_per_line_and_crlf_is_not_matched() {
        let text = Rope::from_str("ab\r\ncd\r\n");
        let re = build(r"^\w+$", true, false).unwrap();
        assert_eq!(rope_matches(&re, &text, false), vec![(0, 2), (4, 6)]);
    }

    #[test]
    fn case_whole_word_and_capture_replacement() {
        let text = Rope::from_str("Foo foo food\n");
        let re = build("foo", false, true).unwrap();
        assert_eq!(rope_matches(&re, &text, false), vec![(0, 3), (4, 7)]);
        let re = build(r"(\w)oo", true, false).unwrap();
        assert_eq!(rope_replacements(&re, &text, false, "$1-0"),
            vec![(0, 3, "F-0".to_owned()), (4, 7, "f-0".to_owned()), (8, 11, "f-0".to_owned())]);
        assert_eq!(replace_in_str(&re, "too\r\nboo\n", false, "${1}x\\t").unwrap(), ("tx\t\r\nbx\t\n".to_owned(), 2));
    }

    #[test]
    fn multiline_patterns_search_the_whole_document() {
        let text = Rope::from_str("a\nb\nc\n");
        assert!(is_multiline(r"a\nb"));
        let re = build(r"a\nb", true, false).unwrap();
        assert_eq!(rope_matches(&re, &text, true), vec![(0, 3)]);
    }

    #[test]
    fn invalid_patterns_report_a_short_error() {
        assert!(build("(unclosed", true, false).is_err());
    }
}
