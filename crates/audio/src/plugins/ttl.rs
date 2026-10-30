//! Just enough Turtle (RDF) to read LV2 bundles: `manifest.ttl` and the
//! plugin descriptions it points to.
//!
//! Covers what LV2 files are written in: `@prefix`/`PREFIX`, `@base`,
//! IRIs, prefixed names, `a`, `;` and `,` lists, blank-node property lists
//! `[ ... ]`, collections `( ... )`, string literals (short and long, with
//! escapes, language tags and datatypes), numbers and booleans. Anything
//! it can't read ends the parse with an error rather than a guess; the
//! scan then skips that bundle.

use std::collections::HashMap;

pub const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
pub const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";

/// An RDF term.
#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    Iri(String),
    Blank(usize),
    /// A literal's lexical form; numbers and booleans included.
    Literal(String),
}

impl Term {
    pub fn iri(&self) -> Option<&str> {
        match self {
            Term::Iri(s) => Some(s),
            _ => None,
        }
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Term::Literal(s) => Some(s),
            _ => None,
        }
    }

    pub fn number(&self) -> Option<f32> {
        self.text()?.trim().parse().ok()
    }
}

/// Triples, as read.
#[derive(Default, Debug)]
pub struct Graph {
    pub triples: Vec<(Term, String, Term)>,
    blanks: usize,
}

impl Graph {
    /// Every object of `subject predicate ?`.
    pub fn objects<'a, 'q>(&'a self, subject: &'q Term, predicate: &'q str) -> impl Iterator<Item = &'a Term> + 'q
    where
        'a: 'q,
    {
        self.triples.iter().filter(move |(s, p, _)| s == subject && p == predicate).map(|(_, _, o)| o)
    }

    /// The first object of `subject predicate ?`.
    pub fn object(&self, subject: &Term, predicate: &str) -> Option<&Term> {
        self.triples.iter().find(|(s, p, _)| s == subject && p == predicate).map(|(_, _, o)| o)
    }

    /// Every subject of `? predicate object`.
    pub fn subjects<'a>(&'a self, predicate: &'a str, object: &'a Term) -> impl Iterator<Item = &'a Term> + 'a {
        self.triples.iter().filter(move |(_, p, o)| p == predicate && o == object).map(|(s, _, _)| s)
    }

    /// Whether `subject` has type `class`.
    pub fn is_a(&self, subject: &Term, class: &str) -> bool {
        self.triples.iter().any(|(s, p, o)| s == subject && p == RDF_TYPE && o.iri() == Some(class))
    }

    /// Reads `text`, whose own address is `base` (relative IRIs resolve
    /// against it), adding its triples.
    pub fn parse(&mut self, text: &str, base: &str) -> Result<(), String> {
        Parser { src: text.as_bytes(), at: 0, base: base.to_string(), prefixes: HashMap::new(), graph: self }.document()
    }
}

struct Parser<'a> {
    src: &'a [u8],
    at: usize,
    base: String,
    prefixes: HashMap<String, String>,
    graph: &'a mut Graph,
}

/// Resolves `iri` against `base` the way LV2 files need: absolute IRIs
/// unchanged, `#frag` and file names relative to the base's directory.
fn resolve(base: &str, iri: &str) -> String {
    if iri.contains(':') {
        return iri.to_string();
    }
    if iri.is_empty() {
        return base.to_string();
    }
    if let Some(frag) = iri.strip_prefix('#') {
        let doc = base.split('#').next().unwrap_or(base);
        return format!("{doc}#{frag}");
    }
    let dir = match base.rfind('/') {
        Some(i) => &base[..=i],
        None => base,
    };
    format!("{dir}{iri}")
}

impl Parser<'_> {
    fn err<T>(&self, what: &str) -> Result<T, String> {
        let line = self.src[..self.at.min(self.src.len())].iter().filter(|&&b| b == b'\n').count() + 1;
        Err(format!("line {line}: {what}"))
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.at).copied()
    }

    /// Skips whitespace and comments.
    fn space(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.at += 1;
            } else if c == b'#' {
                while self.peek().is_some_and(|c| c != b'\n') {
                    self.at += 1;
                }
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.space();
        if self.peek() == Some(c) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            self.err(&format!("expected '{}'", c as char))
        }
    }

    fn starts_with_word(&self, word: &str) -> bool {
        let rest = &self.src[self.at..];
        rest.len() >= word.len()
            && rest[..word.len()].eq_ignore_ascii_case(word.as_bytes())
            && rest.get(word.len()).is_none_or(|c| c.is_ascii_whitespace() || *c == b'<')
    }

    fn document(&mut self) -> Result<(), String> {
        loop {
            self.space();
            if self.peek().is_none() {
                return Ok(());
            }
            if self.starts_with_word("@prefix") || self.starts_with_word("prefix") {
                let sparql = self.peek() != Some(b'@');
                self.at += if sparql { 6 } else { 7 };
                self.space();
                let name = self.name_until(b':')?;
                self.at += 1;
                self.space();
                let iri = self.iri_ref()?;
                self.prefixes.insert(name, iri);
                if !sparql {
                    self.expect(b'.')?;
                }
            } else if self.starts_with_word("@base") || self.starts_with_word("base") {
                let sparql = self.peek() != Some(b'@');
                self.at += if sparql { 4 } else { 5 };
                self.space();
                self.base = self.iri_ref()?;
                if !sparql {
                    self.expect(b'.')?;
                }
            } else {
                self.statement()?;
            }
        }
    }

    /// Reads the characters of a prefix name up to `end`.
    fn name_until(&mut self, end: u8) -> Result<String, String> {
        let start = self.at;
        while let Some(c) = self.peek() {
            if c == end {
                return Ok(String::from_utf8_lossy(&self.src[start..self.at]).into_owned());
            }
            if c.is_ascii_whitespace() {
                break;
            }
            self.at += 1;
        }
        self.err("bad prefix name")
    }

    fn statement(&mut self) -> Result<(), String> {
        self.space();
        let subject = if self.peek() == Some(b'[') {
            self.at += 1;
            let node = self.new_blank();
            if !self.eat(b']') {
                self.predicate_objects(&node)?;
                self.expect(b']')?;
            }
            // `[ ... ] .` on its own is allowed.
            if self.eat(b'.') {
                return Ok(());
            }
            node
        } else {
            self.subject()?
        };
        self.predicate_objects(&subject)?;
        self.expect(b'.')
    }

    fn new_blank(&mut self) -> Term {
        self.graph.blanks += 1;
        Term::Blank(self.graph.blanks)
    }

    fn subject(&mut self) -> Result<Term, String> {
        self.space();
        match self.peek() {
            Some(b'(') => self.collection(),
            Some(b'_') => self.blank_label(),
            _ => Ok(Term::Iri(self.iri()?)),
        }
    }

    fn blank_label(&mut self) -> Result<Term, String> {
        // `_:name` -- a labelled blank node; labels map to fresh numbers
        // per document through the prefix table under a reserved key.
        self.at += 2;
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
            self.at += 1;
        }
        let key = format!("\0_:{}", String::from_utf8_lossy(&self.src[start..self.at]));
        if let Some(n) = self.prefixes.get(&key) {
            return Ok(Term::Blank(n.parse().unwrap_or(0)));
        }
        let node = self.new_blank();
        if let Term::Blank(n) = node {
            self.prefixes.insert(key, n.to_string());
        }
        Ok(node)
    }

    fn predicate_objects(&mut self, subject: &Term) -> Result<(), String> {
        loop {
            self.space();
            let predicate = if self.peek() == Some(b'a') && self.src.get(self.at + 1).is_some_and(u8::is_ascii_whitespace) {
                self.at += 1;
                RDF_TYPE.to_string()
            } else {
                self.iri()?
            };
            loop {
                let object = self.object()?;
                self.graph.triples.push((subject.clone(), predicate.clone(), object));
                if !self.eat(b',') {
                    break;
                }
            }
            if !self.eat(b';') {
                return Ok(());
            }
            // A trailing `;` before `.` or `]` is allowed.
            while self.eat(b';') {}
            self.space();
            if matches!(self.peek(), Some(b'.' | b']')) {
                return Ok(());
            }
        }
    }

    fn object(&mut self) -> Result<Term, String> {
        self.space();
        match self.peek() {
            Some(b'[') => {
                self.at += 1;
                let node = self.new_blank();
                if !self.eat(b']') {
                    self.predicate_objects(&node)?;
                    self.expect(b']')?;
                }
                Ok(node)
            }
            Some(b'(') => self.collection(),
            Some(b'_') if self.src.get(self.at + 1) == Some(&b':') => self.blank_label(),
            Some(b'"' | b'\'') => self.literal(),
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' => Ok(Term::Literal(self.number())),
            _ if self.starts_with_bare("true") => {
                self.at += 4;
                Ok(Term::Literal("true".into()))
            }
            _ if self.starts_with_bare("false") => {
                self.at += 5;
                Ok(Term::Literal("false".into()))
            }
            _ => Ok(Term::Iri(self.iri()?)),
        }
    }

    fn starts_with_bare(&self, word: &str) -> bool {
        let rest = &self.src[self.at..];
        rest.starts_with(word.as_bytes()) && rest.get(word.len()).is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b':')
    }

    fn collection(&mut self) -> Result<Term, String> {
        self.at += 1;
        let mut items = Vec::new();
        while !self.eat(b')') {
            if self.peek().is_none() {
                return self.err("unclosed collection");
            }
            items.push(self.object()?);
        }
        let nil = Term::Iri(format!("{RDF}nil"));
        let mut head = nil;
        for item in items.into_iter().rev() {
            let node = self.new_blank();
            self.graph.triples.push((node.clone(), format!("{RDF}first"), item));
            self.graph.triples.push((node.clone(), format!("{RDF}rest"), head));
            head = node;
        }
        Ok(head)
    }

    fn number(&mut self) -> String {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E')) {
            // A '.' followed by whitespace ends the statement, not the number.
            if self.peek() == Some(b'.') && !self.src.get(self.at + 1).is_some_and(u8::is_ascii_digit) {
                break;
            }
            self.at += 1;
        }
        String::from_utf8_lossy(&self.src[start..self.at]).into_owned()
    }

    fn literal(&mut self) -> Result<Term, String> {
        let quote = self.peek().expect("checked");
        let long = self.src[self.at..].starts_with(&[quote; 3]);
        self.at += if long { 3 } else { 1 };
        let mut out = Vec::new();
        loop {
            let Some(c) = self.peek() else { return self.err("unclosed string") };
            if long && self.src[self.at..].starts_with(&[quote; 3]) {
                self.at += 3;
                break;
            }
            if !long && c == quote {
                self.at += 1;
                break;
            }
            if !long && c == b'\n' {
                return self.err("newline in string");
            }
            self.at += 1;
            if c == b'\\' {
                let Some(e) = self.peek() else { return self.err("bad escape") };
                self.at += 1;
                match e {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'u' | b'U' => {
                        let n = if e == b'u' { 4 } else { 8 };
                        let hex = std::str::from_utf8(self.src.get(self.at..self.at + n).unwrap_or_default()).unwrap_or("");
                        let ch = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
                        let Some(ch) = ch else { return self.err("bad \\u escape") };
                        self.at += n;
                        out.extend_from_slice(ch.to_string().as_bytes());
                    }
                    other => out.push(other),
                }
            } else {
                out.push(c);
            }
        }
        // Language tag or datatype: read and dropped, the lexical form is
        // what LV2 hosts use.
        if self.peek() == Some(b'@') {
            while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'@') {
                self.at += 1;
            }
        } else if self.src[self.at..].starts_with(b"^^") {
            self.at += 2;
            self.iri()?;
        }
        Ok(Term::Literal(String::from_utf8_lossy(&out).into_owned()))
    }

    /// `<...>`, resolved against the base.
    fn iri_ref(&mut self) -> Result<String, String> {
        self.space();
        if self.peek() != Some(b'<') {
            return self.err("expected '<'");
        }
        self.at += 1;
        let start = self.at;
        while self.peek().is_some_and(|c| c != b'>') {
            self.at += 1;
        }
        if self.peek().is_none() {
            return self.err("unclosed IRI");
        }
        let raw = String::from_utf8_lossy(&self.src[start..self.at]).into_owned();
        self.at += 1;
        Ok(resolve(&self.base, &raw))
    }

    /// An IRI written either way.
    fn iri(&mut self) -> Result<String, String> {
        self.space();
        if self.peek() == Some(b'<') {
            return self.iri_ref();
        }
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b':' | b'.' | b'%') || c >= 0x80) {
            self.at += 1;
        }
        // A name never ends with '.', which is the statement's end.
        while self.at > start && self.src[self.at - 1] == b'.' {
            self.at -= 1;
        }
        let name = String::from_utf8_lossy(&self.src[start..self.at]).into_owned();
        let Some((prefix, local)) = name.split_once(':') else { return self.err(&format!("not an IRI: '{name}'")) };
        match self.prefixes.get(prefix) {
            Some(ns) => Ok(format!("{ns}{local}")),
            None => self.err(&format!("unknown prefix '{prefix}:'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LV2: &str = "http://lv2plug.in/ns/lv2core#";

    #[test]
    fn reads_an_lv2_plugin_description() {
        let text = r#"
            @prefix lv2:  <http://lv2plug.in/ns/lv2core#> .
            @prefix doap: <http://usefulinc.com/ns/doap#> .
            PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
            # A gain plugin.
            <urn:test:amp>
                a lv2:Plugin , lv2:AmplifierPlugin ;
                doap:name "Simple \"Amp\""@en ;
                lv2:requiredFeature <http://lv2plug.in/ns/ext/urid#map> ;
                lv2:port [
                    a lv2:InputPort , lv2:ControlPort ;
                    lv2:index 0 ;
                    lv2:symbol "gain" ;
                    lv2:default 0.0 ;
                    lv2:minimum -90.0 ;
                    lv2:maximum 24.0e0 ;
                    lv2:portProperty lv2:toggled ;
                ] , [
                    a lv2:AudioPort , lv2:InputPort ;
                    lv2:index 1 ;
                    lv2:symbol "in"
                ] ;
                rdfs:comment """two
            lines""" .
        "#;
        let mut g = Graph::default();
        g.parse(text, "file:///b/amp.ttl").unwrap();
        let amp = Term::Iri("urn:test:amp".into());
        assert!(g.is_a(&amp, &format!("{LV2}Plugin")));
        assert!(g.is_a(&amp, &format!("{LV2}AmplifierPlugin")));
        assert_eq!(g.object(&amp, "http://usefulinc.com/ns/doap#name").and_then(Term::text), Some("Simple \"Amp\""));
        let ports: Vec<&Term> = g.objects(&amp, &format!("{LV2}port")).collect();
        assert_eq!(ports.len(), 2);
        assert_eq!(g.object(ports[0], &format!("{LV2}minimum")).and_then(Term::number), Some(-90.0));
        assert_eq!(g.object(ports[0], &format!("{LV2}maximum")).and_then(Term::number), Some(24.0));
        assert_eq!(g.object(ports[1], &format!("{LV2}symbol")).and_then(Term::text), Some("in"));
        assert_eq!(g.object(&amp, &format!("{RDFS}comment")).and_then(Term::text), Some("two\n            lines"));
    }

    #[test]
    fn relative_iris_resolve_against_the_file() {
        let mut g = Graph::default();
        g.parse(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n<urn:x> lv2:binary <amp.so> ; <#p> ( 1 2 ) .",
            "file:///usr/lib/lv2/amp.lv2/manifest.ttl",
        )
        .unwrap();
        let x = Term::Iri("urn:x".into());
        assert_eq!(g.object(&x, &format!("{LV2}binary")).and_then(Term::iri), Some("file:///usr/lib/lv2/amp.lv2/amp.so"));
        let list = g.object(&x, "file:///usr/lib/lv2/amp.lv2/manifest.ttl#p").unwrap().clone();
        assert_eq!(g.object(&list, &format!("{RDF}first")).and_then(Term::number), Some(1.0));
    }

    #[test]
    fn errors_are_reported_not_guessed() {
        let mut g = Graph::default();
        assert!(g.parse("<urn:x> nope:y <urn:z> .", "file:///a").unwrap_err().contains("unknown prefix"));
        assert!(Graph::default().parse("<urn:x> <urn:y> \"open", "file:///a").is_err());
    }
}
