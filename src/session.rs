// Session restore: which files were open, in which split layout, and where the
// cursor was — saved on quit (and periodically, so a crash loses little) and
// reopened on the next launch.
//
// Sessions are stored per workspace in `<data dir>/sessions/<hash>.json`, keyed by
// the workspace root (or "" when no folder is open). Launching with no arguments
// reopens the most recently used workspace (`last.json` names it). Only file tabs
// are kept: terminals, previews and untitled buffers are not (unsaved text is
// protected separately by the recovery files).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SessionTab {
    /// `VPath` string form (local path or `ssh://…`).
    pub path:   String,
    pub cursor: usize,
    pub scroll: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SessionNode {
    Leaf { tabs: Vec<SessionTab>, active: usize },
    Split { vertical: bool, ratio: f32, a: Box<SessionNode>, b: Box<SessionNode> },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Session {
    pub version: u32,
    /// Workspace root (`VPath` string), if a folder was open.
    pub root:    Option<String>,
    pub layout:  Option<SessionNode>,
    /// Index (in depth-first leaf order) of the focused pane.
    pub active_leaf: usize,
}

fn dir() -> Option<PathBuf> {
    let d = crate::os::data_dir()?.join("sessions");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

fn file_for(key: &str) -> Option<PathBuf> {
    // FNV-1a: stable across runs, no dependency.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in key.as_bytes() { h ^= b as u64; h = h.wrapping_mul(0x0000_0100_0000_01b3); }
    Some(dir()?.join(format!("{h:016x}.json")))
}

/// Write the session for workspace `key` and remember it as the last one used.
pub fn save(key: &str, session: &Session) {
    let (Some(path), Some(d)) = (file_for(key), dir()) else { return };
    let Ok(text) = serde_json::to_string_pretty(session) else { return };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let _ = std::fs::write(d.join("last.json"), serde_json::json!({ "key": key }).to_string());
}

pub fn load(key: &str) -> Option<Session> {
    let text = std::fs::read_to_string(file_for(key)?).ok()?;
    serde_json::from_str::<Session>(&text).ok().filter(|s| s.version == 1)
}

/// The workspace key of the last saved session.
pub fn last_key() -> Option<String> {
    let text = std::fs::read_to_string(dir()?.join("last.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["key"].as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_round_trip_through_json() {
        let s = Session {
            version: 1,
            root: Some("/work".into()),
            layout: Some(SessionNode::Split {
                vertical: true, ratio: 0.4,
                a: Box::new(SessionNode::Leaf { tabs: vec![SessionTab { path: "/work/a.rs".into(), cursor: 10, scroll: 2 }], active: 0 }),
                b: Box::new(SessionNode::Leaf { tabs: vec![], active: 0 }),
            }),
            active_leaf: 1,
        };
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Session>(&text).unwrap(), s);
    }
}
