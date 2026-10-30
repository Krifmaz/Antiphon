//! Theme files: a theme, an accent and a reading font, saved in Stoop's
//! theme markup (see [`crate::markup`]) so they can be shared and loaded
//! again. A file looks like this:
//!
//! ```text
//! stoop-theme {
//!     version: 1;
//!     theme: dark;
//!     accent: custom;
//!     custom-accent: #336699;
//!     font: inter;
//! }
//! ```
//!
//! Files exported as JSON by earlier builds still load: text that starts
//! with `{` is read as the old JSON form.
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

/// What the JSON form's `format` field must say, and the markup form's
/// element name, so a stray file is not taken for a theme.
pub const FORMAT: &str = "stoop-theme";

/// The newest file version this build reads and the one it writes.
pub const VERSION: u32 = 1;

/// The name a saved theme file is offered under.
pub const FILE_NAME: &str = "my-theme.stoop-theme";

/// The extensions the import dialog offers: the markup form, and `json` for
/// files exported by earlier builds.
pub const EXTENSIONS: [&str; 2] = ["stoop-theme", "json"];

/// Files bigger than this are refused before parsing: a real theme file is
/// a few hundred bytes.
pub const MAX_BYTES: usize = 16 * 1024;

/// One theme file's contents. The serde attributes describe the old JSON
/// form, which is still read; the markup form is read and written by hand
/// in [`ThemeFile::parse`] and [`ThemeFile::to_markup`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ThemeFile {
    /// Always [`FORMAT`] once parsed.
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

    /// The file as theme markup, ready to save or copy.
    pub fn to_markup(&self) -> String {
        use crate::markup::{write, Document, Value};
        let mut props = vec![
            ("version".to_string(), Value::Number(self.version as f64)),
            ("theme".to_string(), Value::Ident(self.theme.clone())),
            ("accent".to_string(), Value::Ident(self.accent.clone())),
        ];
        // Written only with the custom accent, as the JSON form did.
        if let Some(c) = crate::theme::parse_hex(&self.custom_accent).filter(|_| !self.custom_accent.is_empty()) {
            props.push(("custom-accent".to_string(), Value::Color(c)));
        }
        props.push(("font".to_string(), Value::Ident(self.font.clone())));
        let doc = Document { element: FORMAT.to_string(), props };
        write(&doc, &["A Stoop theme: colours and a font, nothing that runs.", "Load it with Settings > Appearance > Import theme."])
    }

    /// Reads and checks a theme file in either form. The error says what is
    /// wrong in words the person can act on.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES {
            return Err("That file is too big to be a theme file.".into());
        }
        let file = if text.trim_start().starts_with('{') {
            serde_json::from_str::<ThemeFile>(text).map_err(|_| "That isn't a Stoop theme file.".to_string())?
        } else {
            Self::from_markup(text)?
        };
        file.check()?;
        Ok(file)
    }

    /// Reads the markup form. Properties this version doesn't know are
    /// dropped, as unknown JSON fields are; values are checked afterwards
    /// by [`ThemeFile::check`].
    fn from_markup(text: &str) -> Result<Self, String> {
        let doc = crate::markup::parse(text).map_err(|e| format!("That isn't a Stoop theme file. {e}"))?;
        if doc.element != FORMAT {
            return Err("That isn't a Stoop theme file.".into());
        }
        // A required word, or an error naming the missing property.
        let word = |k: &str| doc.text(k).map(str::to_string).ok_or_else(|| format!("The theme file has no {k}."));
        Ok(ThemeFile {
            format: FORMAT.to_string(),
            version: doc.uint("version").ok_or("The theme file has no version.")?,
            theme: word("theme")?,
            accent: word("accent")?,
            custom_accent: doc.color("custom-accent").map(crate::theme::to_hex).unwrap_or_default(),
            font: doc.text("font").map(str::to_string).unwrap_or_else(default_font),
        })
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
        assert_eq!(ThemeFile::parse(&f.to_markup()), Ok(f));
        let f = ThemeFile::new("oled", "teal", "#336699", "inter");
        assert!(f.custom_accent.is_empty(), "a built-in accent carries no custom colour");
        assert_eq!(ThemeFile::parse(&f.to_markup()), Ok(f));
    }

    #[test]
    fn an_exported_file_is_markup() {
        let text = ThemeFile::new("dark", "custom", "#336699", "inter").to_markup();
        assert!(text.contains("stoop-theme {"), "{text}");
        assert!(text.contains("custom-accent: #336699;"), "{text}");
    }

    #[test]
    fn hand_written_markup_loads() {
        let f = ThemeFile::parse("// mine\nstoop-theme { version: 1; theme: \"oled\"; accent: teal; }").unwrap();
        assert_eq!(f, ThemeFile::new("oled", "teal", "", "inter"));
    }

    #[test]
    fn bad_markup_is_refused() {
        let bad = [
            "other { version: 1; theme: dark; accent: violet; }",
            "stoop-theme { version: 2; theme: dark; accent: violet; }",
            "stoop-theme { theme: dark; accent: violet; }",
            "stoop-theme { version: 1; theme: neon; accent: violet; }",
            "stoop-theme { version: 1; theme: dark; accent: custom; }",
            "stoop-theme { version: 1; theme: dark; accent: violet; font: comic; }",
            "stoop-theme { version: 1; theme: dark; accent: violet;",
        ];
        for text in bad {
            assert!(ThemeFile::parse(text).is_err(), "{text} should be refused");
        }
        // Unknown properties are dropped, never acted on.
        let f = ThemeFile::parse("stoop-theme { version: 1; theme: dark; accent: violet; script: \"rm -rf /\"; }").unwrap();
        assert_eq!(f, ThemeFile::new("dark", "violet", "", "inter"));
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
            "{ not json",
            "not a theme",
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
