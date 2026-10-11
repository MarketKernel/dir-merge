use serde::{Deserialize, Serialize};

use crate::diff::DiffOptions;
use crate::dircmp::DirOptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemePref {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentPair {
    pub left: String,
    pub right: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Bumped when defaults change in a way saved settings should pick up.
    /// Missing in old saves, so it reads as 0 there.
    #[serde(default)]
    pub version: u32,
    pub theme: ThemePref,
    pub ui_font_size: f32,
    pub code_font_size: f32,
    pub tab_width: usize,
    pub show_whitespace: bool,
    pub syntax_highlighting: bool,
    pub dark_syntax_theme: String,
    pub light_syntax_theme: String,
    pub diff: DiffOptions,
    pub dir: DirOptions,
    pub show_identical: bool,
    pub show_different: bool,
    pub show_left_only: bool,
    pub show_right_only: bool,
    pub recent: Vec<RecentPair>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: Settings::VERSION,
            theme: ThemePref::System,
            ui_font_size: 15.0,
            code_font_size: 14.0,
            tab_width: 4,
            show_whitespace: false,
            syntax_highlighting: true,
            dark_syntax_theme: "OneHalfDark".into(),
            light_syntax_theme: "OneHalfLight".into(),
            diff: DiffOptions::default(),
            dir: DirOptions::default(),
            show_identical: true,
            show_different: true,
            show_left_only: true,
            show_right_only: true,
            recent: Vec::new(),
        }
    }
}

impl Settings {
    pub const KEY: &'static str = "dir-merge-settings";
    const VERSION: u32 = 1;

    /// Brings settings saved by an older build up to date.
    pub fn migrate(&mut self) {
        if self.version < 1 {
            // Font sizes still at the old defaults follow the new, larger ones.
            let d = Settings::default();
            if self.ui_font_size == 13.5 {
                self.ui_font_size = d.ui_font_size;
            }
            if self.code_font_size == 13.0 {
                self.code_font_size = d.code_font_size;
            }
        }
        self.version = Settings::VERSION;
    }

    pub fn add_recent(&mut self, left: &str, right: &str) {
        let pair = RecentPair { left: left.to_string(), right: right.to_string() };
        self.recent.retain(|p| p != &pair);
        self.recent.insert(0, pair);
        self.recent.truncate(12);
    }
}
