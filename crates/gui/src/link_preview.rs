//! Link handling in chat (spec §13: "link previews -- privacy trap:
//! fetching a preview leaks your IP to that website. Should be
//! sender-generated or off by default").
//!
//! The spec names the trap precisely, so this module takes the side it
//! points at: **nothing is ever fetched.** A receiver-generated preview
//! means every link anyone sends you causes your machine to contact a
//! server chosen by them, before you have decided whether you trust the
//! link. That turns a chat message into an IP-address probe, and it is
//! worth being blunt that no amount of "just for the thumbnail" changes
//! what it is.
//!
//! What is offered instead is entirely local: find the links in a message
//! and show what they actually point at, so a reader can judge before
//! clicking. That is the useful half of a preview -- knowing where you are
//! about to go -- without the half that phones home.
//!
//! Deliberately not implemented, and not planned here:
//!
//! - fetching page titles, favicons or images
//! - unfurling shortened links (which is a fetch wearing a hat)
//! - sender-generated previews, which would be safe for the receiver but
//!   need a way to carry an image with a message; that waits on §8

/// A link found in a message.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Link {
    /// Exactly as it appeared in the message.
    pub raw: String,
    /// The host the link points at, lowercased.
    pub host: String,
    /// True when the link is plain HTTP, so the UI can say so.
    pub insecure: bool,
}

/// Finds http/https links in a message.
///
/// Deliberately conservative: it only recognises links that announce
/// themselves with a scheme. Guessing that `example.com` is a link means
/// guessing wrong on ordinary sentences, and a false positive here is a
/// warning about a link that does not exist.
pub fn find_links(text: &str) -> Vec<Link> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        // Trailing punctuation is part of the sentence, not the URL.
        let trimmed = token.trim_end_matches(['.', ',', ')', ']', '!', '?', ';', ':', '"', '\'']);
        let lower = trimmed.to_lowercase();

        let rest = if let Some(r) = lower.strip_prefix("https://") {
            Some((r, false))
        } else {
            lower.strip_prefix("http://").map(|r| (r, true))
        };
        let Some((rest, insecure)) = rest else { continue };

        // Host runs to the first path, query, or fragment separator.
        let host: String = rest
            .chars()
            .take_while(|c| !matches!(c, '/' | '?' | '#'))
            .collect();
        // Strip userinfo: `https://real-looking.com@evil.example` points at
        // `evil.example`, and showing the part before the `@` would help
        // the deception rather than expose it.
        let host = host.rsplit('@').next().unwrap_or("").to_string();
        if host.is_empty() || !host.contains('.') {
            continue;
        }

        out.push(Link { raw: trimmed.to_string(), host, insecure });
    }
    out
}

/// A one-line summary of the links in a message, for showing under it.
///
/// Returns an empty string when there are none, so the caller can hide the
/// row entirely rather than showing an empty label.
pub fn describe(text: &str) -> String {
    let links = find_links(text);
    if links.is_empty() {
        return String::new();
    }

    let mut hosts: Vec<String> = Vec::new();
    for link in &links {
        let label = if link.insecure {
            format!("{} (not encrypted)", link.host)
        } else {
            link.host.clone()
        };
        if !hosts.contains(&label) {
            hosts.push(label);
        }
    }

    if hosts.len() == 1 {
        format!("Link to {}", hosts[0])
    } else {
        format!("Links to {}", hosts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_https_link_is_found() {
        let links = find_links("have a look at https://example.org/page");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].host, "example.org");
        assert!(!links[0].insecure);
    }

    #[test]
    fn http_is_flagged_as_not_encrypted() {
        let links = find_links("http://example.org");
        assert!(links[0].insecure);
        assert!(describe("http://example.org").contains("not encrypted"));
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_link() {
        // "Look at https://example.org." -- the full stop ends the
        // sentence, it is not part of the address.
        let links = find_links("look at https://example.org.");
        assert_eq!(links[0].host, "example.org");
        assert_eq!(links[0].raw, "https://example.org");
    }

    #[test]
    fn a_userinfo_trick_shows_the_real_destination() {
        // `https://paypal.com@evil.example` goes to evil.example. Showing
        // the part before the @ would help the deception along.
        let links = find_links("https://paypal.com@evil.example/login");
        assert_eq!(links[0].host, "evil.example");
    }

    #[test]
    fn ordinary_words_are_not_guessed_to_be_links() {
        // Guessing that a bare word is a domain means warning about links
        // that do not exist.
        for text in ["meet me at example.com", "e.g. tomorrow", "see you at 3.30", "no links here"] {
            assert!(find_links(text).is_empty(), "false positive in {text:?}");
        }
    }

    #[test]
    fn several_links_are_all_reported() {
        let text = "https://one.example and https://two.example";
        assert_eq!(find_links(text).len(), 2);
        let described = describe(text);
        assert!(described.contains("one.example"));
        assert!(described.contains("two.example"));
    }

    #[test]
    fn the_same_host_twice_is_only_named_once() {
        let described = describe("https://example.org/a https://example.org/b");
        assert_eq!(described, "Link to example.org");
    }

    #[test]
    fn a_message_with_no_links_describes_as_nothing() {
        // So the caller hides the row rather than showing an empty label.
        assert!(describe("just a normal message").is_empty());
        assert!(describe("").is_empty());
    }

    #[test]
    fn a_scheme_with_no_host_is_ignored() {
        for text in ["https://", "http://", "https:///path", "https://nodot/path"] {
            assert!(find_links(text).is_empty(), "accepted {text:?}");
        }
    }

    #[test]
    fn nothing_here_performs_any_network_access() {
        // Not something a test can assert directly, so it is stated: this
        // module has no HTTP client, no DNS lookup and no dependency that
        // could perform one. `describe` is pure string handling, which is
        // the entire point -- see the module docs.
        let described = describe("https://example.org/thing");
        assert_eq!(described, "Link to example.org");
    }
}
