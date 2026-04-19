// Persistent settings — serialized to/from ~/.config/local-text/config.json.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RendererBackend { Cpu, Gpu }

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub renderer:  RendererBackend,
    pub vsync:     bool,
    #[serde(default = "Settings::default_font_size")]
    pub font_size: f32,
}

impl Default for Settings {
    fn default() -> Self { Settings { renderer: RendererBackend::Cpu, vsync: true, font_size: Self::default_font_size() } }
}

impl Settings {
    pub fn default_font_size() -> f32 { 14.0 }

    fn config_path() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        let dir  = PathBuf::from(home).join(".config").join("local-text");
        fs::create_dir_all(&dir).ok()?;
        Some(dir.join("config.json"))
    }

    pub fn load() -> Self {
        let path = match Self::config_path() { Some(p) => p, None => return Self::default() };
        let text = match fs::read_to_string(&path) { Ok(t) => t, Err(_) => return Self::default() };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::config_path() else { return };
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, text);
        }
    }
}
