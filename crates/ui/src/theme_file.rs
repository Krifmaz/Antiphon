//! Theme files: a theme, an accent and a reading font, saved as plain JSON
//! so they can be shared and loaded again.
//!
//! A theme file is declarative data and nothing else. It names a theme and
//! an accent from the built-in tables (or a custom accent colour) and a
//! reading font; it cannot carry code, paths, pictures or anything that
//! would run or be fetched. Loading one is parsing and checking, never
//! executing, and every value is checked against what Settings itself
//! offers, so a file can only ever select a look the app could select by
//! hand.

use serde::{Deserialize, Serialize};

use crate::theme::{self, ACCENTS, FONTS, THEMES};

/// What the `format` field must say, so a stray JSON file is not taken for
/// a theme.
pub const FORMAT: &str = "stoop-theme";

/// The newest file version this build reads and the one it writes.
pub const VERSION: u32 = 1;

/// The name a saved theme file is offered under.
pub const FILE_NAME: &str = "my-theme.stoop-theme.json";

/// Files bigger than this are refused before parsing: a real theme file is
/// a few hundred bytes.
pub const MAX_BYTES: usize = 16 * 1024;

/// One theme file's contents.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ThemeFile {
    /// Always [`FORMAT`].
    pub format: String,
    /// The file version, [`VERSION`] when written by this build.
    pub version: u32,
    /// A theme id from [`THEMES`].
    pub theme: String,
    /// An accent id from [`ACCENTS`], or `custom`.
    pub accent: String,
    /// The custom accent as `#rrggbb`; only read when `accent` is `custom`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub custom_accent: String,
    /// A reading-font id from [`FONTS`]; absent means the default, Inter.
    #[serde(default = "default_font")]
    pub font: String,
}

/// The reading font a file without one gets.
fn default_font() -> String {
    FONTS[0].0.to_string()
}

impl ThemeFile {
    /// A theme file for these settings, as the app would write it.
    pub fn new(theme: &str, accent: &str, custom_accent: &str, font: &str) -> Self {
        ThemeFile {
            format: FORMAT.to_string(),
            version: VERSION,
            theme: theme.to_string(),
            accent: accent.to_string(),
            // A custom colour only means something with the custom accent.
            custom_accent: if accent == "custom" { custom_accent.to_string() } else { String::new() },
            font: font.to_string(),
        }
    }

    /// The file as pretty-printed JSON, ready to save.
    pub fn to_json(&self) -> String {
        // Serialising plain strings and a number cannot fail.
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Reads and checks a theme file. The error says what is wrong in words
    /// the person can act on.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES {
            return Err("That file is too big to be a theme file.".into());
        }
        let file: ThemeFile = serde_json::from_str(text).map_err(|_| "That isn't a Stoop theme file.".to_string())?;
        file.check()?;
        Ok(file)
    }

    /// Checks every field against what Settings offers.
    fn check(&self) -> Result<(), String> {
        if self.format != FORMAT {
            return Err("That isn't a Stoop theme file.".into());
        }
        if self.version > VERSION {
            return Err("That theme file was made by a newer Stoop; update to load it.".into());
        }
        if !THEMES.iter().any(|t| t.0 == self.theme) {
            return Err(format!("The theme file names a theme this Stoop doesn't have: {}.", self.theme));
        }
        if self.accent == "custom" {
            if theme::parse_hex(&self.custom_accent).is_none() {
                return Err("The theme file's custom accent isn't a colour like #8b5cf6.".into());
            }
        } else if !ACCENTS.iter().any(|a| a.0 == self.accent) {
            return Err(format!("The theme file names an accent this Stoop doesn't have: {}.", self.accent));
        }
        if !FONTS.iter().any(|f| f.0 == self.font) {
            return Err(format!("The theme file names a font this Stoop doesn't have: {}.", self.font));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_file_reads_back_the_same() {
        let f = ThemeFile::new("light", "custom", "#336699", "opendyslexic");
        assert_eq!(ThemeFile::parse(&f.to_json()), Ok(f));
        let f = ThemeFile::new("oled", "teal", "#336699", "inter");
        assert!(f.custom_accent.is_empty(), "a built-in accent carries no custom colour");
        assert_eq!(ThemeFile::parse(&f.to_json()), Ok(f));
    }

    #[test]
    fn the_font_may_be_left_out() {
        let f = ThemeFile::parse(r#"{"format":"stoop-theme","version":1,"theme":"dark","accent":"violet"}"#).unwrap();
        assert_eq!(f.font, "inter");
    }

    #[test]
    fn only_known_values_load() {
        let bad = [
            r#"{"format":"other","version":1,"theme":"dark","accent":"violet"}"#,
            r#"{"format":"stoop-theme","version":2,"theme":"dark","accent":"violet"}"#,
            r#"{"format":"stoop-theme","version":1,"theme":"neon","accent":"violet"}"#,
            r#"{"format":"stoop-theme","version":1,"theme":"dark","accent":"plaid"}"#,
            r#"{"format":"stoop-theme","version":1,"theme":"dark","accent":"custom","custom_accent":"blue"}"#,
            r#"{"format":"stoop-theme","version":1,"theme":"dark","accent":"violet","font":"Comic Sans"}"#,
            r#"{"theme":"dark"}"#,
            "not json",
        ];
        for text in bad {
            assert!(ThemeFile::parse(text).is_err(), "{text} should be refused");
        }
    }

    #[test]
    fn unknown_fields_are_ignored_not_run() {
        // A field this version doesn't know is dropped, never acted on.
        let f = ThemeFile::parse(r#"{"format":"stoop-theme","version":1,"theme":"dark","accent":"violet","script":"rm -rf /"}"#).unwrap();
        assert_eq!(f, ThemeFile::new("dark", "violet", "", "inter"));
    }

    #[test]
    fn huge_files_are_refused_unread() {
        let text = format!(r#"{{"format":"stoop-theme","version":1,"theme":"dark","accent":"violet","x":"{}"}}"#, "a".repeat(MAX_BYTES));
        assert!(ThemeFile::parse(&text).is_err());
    }
}
