//! The theme gallery (spec §7: "Presets for people who don't want to
//! build one -- a good default plus a gallery, so customization is an
//! invitation and never a chore").
//!
//! The point of a gallery is that most people never open the colour
//! picker. Somebody who wants to build a palette already can; everyone
//! else wants to click a name and be done. So these are complete, usable
//! looks rather than starting points to be corrected afterwards.
//!
//! Each preset sets only the three values that define a look: the accent,
//! the custom background colour, and whether the theme runs light or dark.
//! Deliberately **not** the wallpaper, font, or opacity -- those are things
//! a person chooses once and would be annoyed to lose because they tried a
//! different colour scheme.

/// One entry in the gallery.
pub struct ThemePreset {
    pub name: &'static str,
    /// `#rrggbb` accent.
    pub accent_hex: &'static str,
    /// `#rrggbb` background, used when `theme_mode` is Custom.
    pub background_hex: &'static str,
    /// 1 light, 2 dark, 3 custom -- matching `AppConfig::theme_mode`.
    pub theme_mode: i32,
    /// One line for the gallery, in the same plain register as the rest of
    /// the settings help.
    pub description: &'static str,
}

/// Theme mode values, matching `AppConfig::theme_mode` and the UI.
const DARK: i32 = 2;
const CUSTOM: i32 = 3;

pub const PRESETS: &[ThemePreset] = &[
    ThemePreset {
        name: "Default",
        accent_hex: "#2d7d5a",
        background_hex: "#202020",
        theme_mode: DARK,
        description: "The green the app ships with, on the system dark palette.",
    },
    ThemePreset {
        name: "Midnight",
        accent_hex: "#5b8dd9",
        background_hex: "#12151c",
        theme_mode: CUSTOM,
        description: "Cool blue on near-black. Easiest on the eyes in a dark room.",
    },
    ThemePreset {
        name: "Ember",
        accent_hex: "#d9743f",
        background_hex: "#1c1614",
        theme_mode: CUSTOM,
        description: "Warm orange on brown-black, for people who find blue light cold.",
    },
    ThemePreset {
        name: "Orchid",
        accent_hex: "#a86fd9",
        background_hex: "#181425",
        theme_mode: CUSTOM,
        description: "Purple on deep violet.",
    },
    ThemePreset {
        name: "Paper",
        accent_hex: "#3b6ea5",
        background_hex: "#f4f1ea",
        theme_mode: CUSTOM,
        description: "Dark text on warm off-white, for bright rooms.",
    },
    ThemePreset {
        name: "High contrast",
        accent_hex: "#ffb000",
        background_hex: "#000000",
        theme_mode: CUSTOM,
        description: "Amber on true black. The strongest separation available, for low vision.",
    },
];

pub fn get(name: &str) -> Option<&'static ThemePreset> {
    PRESETS.iter().find(|p| p.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
        let h = hex.strip_prefix('#')?;
        if h.len() != 6 {
            return None;
        }
        Some((
            u8::from_str_radix(&h[0..2], 16).ok()?,
            u8::from_str_radix(&h[2..4], 16).ok()?,
            u8::from_str_radix(&h[4..6], 16).ok()?,
        ))
    }

    /// Relative luminance, the WCAG definition.
    fn luminance(rgb: (u8, u8, u8)) -> f64 {
        let channel = |c: u8| {
            let c = c as f64 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(rgb.0) + 0.7152 * channel(rgb.1) + 0.0722 * channel(rgb.2)
    }

    fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn every_preset_has_parseable_colours() {
        for p in PRESETS {
            assert!(parse_hex(p.accent_hex).is_some(), "{}: bad accent {}", p.name, p.accent_hex);
            assert!(
                parse_hex(p.background_hex).is_some(),
                "{}: bad background {}",
                p.name,
                p.background_hex
            );
        }
    }

    #[test]
    fn every_accent_is_actually_visible_on_its_background() {
        // A gallery entry that looks broken teaches people the gallery is
        // not worth opening. 3:1 is the WCAG threshold for large text and
        // UI components, which is what an accent is used for here.
        for p in PRESETS {
            let ratio = contrast(
                parse_hex(p.accent_hex).unwrap(),
                parse_hex(p.background_hex).unwrap(),
            );
            assert!(
                ratio >= 3.0,
                "{}: accent {} on {} has only {ratio:.2}:1 contrast",
                p.name,
                p.accent_hex,
                p.background_hex
            );
        }
    }

    #[test]
    fn the_high_contrast_preset_earns_its_name() {
        let p = get("High contrast").unwrap();
        let ratio = contrast(
            parse_hex(p.accent_hex).unwrap(),
            parse_hex(p.background_hex).unwrap(),
        );
        // Well past the 4.5:1 body-text threshold -- this one exists
        // specifically for people who need more than the default.
        assert!(ratio > 10.0, "high contrast preset is only {ratio:.2}:1");
    }

    #[test]
    fn preset_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for p in PRESETS {
            assert!(seen.insert(p.name), "duplicate preset name {}", p.name);
        }
    }

    #[test]
    fn every_listed_name_resolves() {
        for p in PRESETS {
            assert!(get(p.name).is_some(), "{} is listed but does not resolve", p.name);
        }
        assert!(get("Nonexistent").is_none());
    }

    #[test]
    fn every_preset_explains_itself() {
        // Spec §0 "Explain everything" applies to the gallery too: a grid
        // of names with no idea what they look like is a guessing game.
        for p in PRESETS {
            assert!(!p.description.is_empty(), "{} has no description", p.name);
        }
    }

    #[test]
    fn theme_modes_are_ones_the_app_understands() {
        for p in PRESETS {
            assert!(
                (1..=3).contains(&p.theme_mode),
                "{} uses unknown theme mode {}",
                p.name,
                p.theme_mode
            );
        }
    }
}
