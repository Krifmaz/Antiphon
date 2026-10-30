//! A release as GitHub describes it, the file in it this install wants, and
//! the checksum list it is checked against.

use serde::Deserialize;

use crate::target::Target;
use crate::version::Version;

/// The repository Stoop's releases come from.
pub const REPO: &str = "Krifmaz/Stoop";

/// The checksum list every release carries (`.github/workflows/release.yml`).
pub const SUMS: &str = "SHA256SUMS";

/// One file of a release.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Asset {
    /// The file's name (`Stoop-0.3.1-linux-x86_64.AppImage`).
    pub name: String,
    /// Where it downloads from. GitHub redirects this to its file store.
    #[serde(rename = "browser_download_url")]
    pub url: String,
    /// Its size in bytes, as GitHub reports it.
    #[serde(default)]
    pub size: u64,
}

/// A published release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// Its version, read from the tag.
    pub version: Version,
    /// Its title ("Stoop 0.3.1").
    pub name: String,
    /// Its page on GitHub, for reading the notes or downloading by hand.
    pub page: String,
    /// Its files.
    pub assets: Vec<Asset>,
}

/// The fields of GitHub's release object this crate reads.
#[derive(Deserialize)]
struct Raw {
    /// `v0.3.1` or `hotfix-0.3.0.68`.
    tag_name: String,
    /// The title; GitHub sends `null` for an untitled release.
    #[serde(default)]
    name: Option<String>,
    /// The release's page.
    html_url: String,
    /// A draft is not out yet.
    #[serde(default)]
    draft: bool,
    /// A pre-release is not offered as an update.
    #[serde(default)]
    prerelease: bool,
    /// Its files.
    #[serde(default)]
    assets: Vec<Asset>,
}

impl Release {
    /// Reads one release from GitHub's JSON (`GET /repos/{repo}/releases/latest`).
    /// A draft, a pre-release, or a tag that is not a Stoop version is
    /// `Ok(None)`: there is nothing to offer, but nothing went wrong either.
    pub fn from_json(json: &str) -> Result<Option<Release>, String> {
        let raw: Raw = serde_json::from_str(json).map_err(|e| format!("GitHub sent a release that could not be read: {e}"))?;
        if raw.draft || raw.prerelease {
            return Ok(None);
        }
        let Some(version) = Version::parse(&raw.tag_name) else { return Ok(None) };
        Ok(Some(Release { version, name: raw.name.unwrap_or_else(|| format!("Stoop {version}")), page: raw.html_url, assets: raw.assets }))
    }

    /// The file that updates `target`, if this release has one.
    pub fn asset_for(&self, target: &Target) -> Option<&Asset> {
        self.assets.iter().find(|a| target.wants(&a.name))
    }

    /// The release's checksum list, if it has one.
    pub fn sums(&self) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == SUMS)
    }
}

/// The SHA-256 for `name` in a `SHA256SUMS` file, as lowercase hex.
///
/// Lines are what `sha256sum` writes: the hash, a space, then a space (text
/// mode) or `*` (binary mode), then the name. The list also names the bare
/// binaries inside the packages by path (`linux/antiphon`), which never
/// match a release file's name.
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, rest) = line.trim().split_once(' ')?;
        let file = rest.trim_start_matches([' ', '*']);
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hash.to_ascii_lowercase())
    })
}

/// Bytes as lowercase hex, for comparing a digest with a checksum list.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed copy of what GitHub sends for a hotfix.
    const HOTFIX: &str = r#"{
        "tag_name": "hotfix-0.3.0.68", "name": "Stoop 0.3.0.68 (hotfix)",
        "html_url": "https://github.com/Krifmaz/Stoop/releases/tag/hotfix-0.3.0.68",
        "draft": false, "prerelease": false, "body": "notes",
        "assets": [
            {"name": "SHA256SUMS", "browser_download_url": "https://example.invalid/SHA256SUMS", "size": 900, "id": 1},
            {"name": "Stoop-android-0.3.0.68.apk", "browser_download_url": "https://example.invalid/a.apk", "size": 52000000}
        ]
    }"#;

    #[test]
    fn reads_a_release() {
        let r = Release::from_json(HOTFIX).unwrap().unwrap();
        assert_eq!(r.version, Version::parse("0.3.0.68").unwrap());
        assert_eq!(r.name, "Stoop 0.3.0.68 (hotfix)");
        assert_eq!(r.assets.len(), 2);
        assert_eq!(r.sums().unwrap().url, "https://example.invalid/SHA256SUMS");
        assert_eq!(r.asset_for(&Target::Android).unwrap().size, 52_000_000);
        assert_eq!(r.asset_for(&Target::LinuxOther), None);
    }

    #[test]
    fn skips_what_is_not_out() {
        let draft = HOTFIX.replace(r#""draft": false"#, r#""draft": true"#);
        assert_eq!(Release::from_json(&draft).unwrap(), None);
        let pre = HOTFIX.replace(r#""prerelease": false"#, r#""prerelease": true"#);
        assert_eq!(Release::from_json(&pre).unwrap(), None);
        let odd = HOTFIX.replace("hotfix-0.3.0.68\",", "nightly\",");
        assert_eq!(Release::from_json(&odd).unwrap(), None);
        assert!(Release::from_json("not json").is_err());
    }

    #[test]
    fn untitled_releases_get_a_name() {
        let untitled = HOTFIX.replace(r#""name": "Stoop 0.3.0.68 (hotfix)""#, r#""name": null"#);
        assert_eq!(Release::from_json(&untitled).unwrap().unwrap().name, "Stoop 0.3.0.68");
    }

    #[test]
    fn finds_a_checksum() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!("{a}  Stoop-0.3.1-linux-x86_64.AppImage\n{b} *Stoop-android-0.3.1.apk\n{a}  linux/antiphon\nnonsense\n");
        assert_eq!(sum_for(&sums, "Stoop-0.3.1-linux-x86_64.AppImage"), Some(a.clone()));
        assert_eq!(sum_for(&sums, "Stoop-android-0.3.1.apk"), Some("b".repeat(64)));
        assert_eq!(sum_for(&sums, "antiphon"), None);
        assert_eq!(sum_for("abc  short", "short"), None);
    }

    #[test]
    fn writes_hex() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }
}
