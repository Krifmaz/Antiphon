//! Stoop's theme markup: a small declarative format of nested elements
//! and properties, used both for the themes built into the app and for the theme
//! files people export and share.
//!
//! A document is one element with a name and a block of properties:
//!
//! ```text
//! // Comments run to the end of the line, /* or across lines */.
//! stoop-theme {
//!     name: "Midnight";
//!     dark: true;
//!     bg: #0b0b12;
//!     avatars: [#c7aef5, #9fd2fb];
//! }
//! ```
//!
//! A value is a quoted string, a `#rgb`/`#rrggbb` colour, a number, `true`
//! or `false`, a bare word, or a flat `[...]` list of those. That is the
//! whole language: there are no expressions, bindings, imports or
//! callbacks, so reading a document is parsing and nothing else, and no
//! file can make the app run or fetch anything.

use iced_core::Color;

use crate::theme::parse_hex;

/// Documents with more properties than this are refused; a theme has a few
/// dozen at most.
const MAX_PROPS: usize = 256;

/// Lists with more items than this are refused.
const MAX_LIST: usize = 64;

/// One property value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A quoted string, with its escapes already resolved.
    Str(String),
    /// A `#rgb` or `#rrggbb` colour.
    Color(Color),
    /// A number, integer or decimal.
    Number(f64),
    /// `true` or `false`.
    Bool(bool),
    /// A bare word such as `dark` or `opendyslexic`.
    Ident(String),
    /// A flat list; lists do not nest.
    List(Vec<Value>),
}

/// A parsed document: the element's name and its properties in file order.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    /// The element's name, such as `stoop-theme`.
    pub element: String,
    /// `(name, value)` pairs in the order they were written. Names are
    /// unique: a repeated name is a parse error.
    pub props: Vec<(String, Value)>,
}

impl Document {
    /// The value of property `name`, if the document has one.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.props.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// Property `name` as text: a quoted string or a bare word.
    pub fn text(&self, name: &str) -> Option<&str> {
        match self.get(name)? {
            Value::Str(s) | Value::Ident(s) => Some(s),
            _ => None,
        }
    }

    /// Property `name` as a colour.
    pub fn color(&self, name: &str) -> Option<Color> {
        match self.get(name)? {
            Value::Color(c) => Some(*c),
            _ => None,
        }
    }

    /// Property `name` as a whole, non-negative number.
    pub fn uint(&self, name: &str) -> Option<u32> {
        match self.get(name)? {
            // Only exact whole numbers in range; `1.5` is not a version.
            Value::Number(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= u32::MAX as f64 => Some(*n as u32),
            _ => None,
        }
    }

    /// Property `name` as `true` or `false`.
    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.get(name)? {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Property `name` as a list of colours; `None` if any item isn't one.
    pub fn colors(&self, name: &str) -> Option<Vec<Color>> {
        match self.get(name)? {
            Value::List(items) => items.iter().map(|v| if let Value::Color(c) = v { Some(*c) } else { None }).collect(),
            _ => None,
        }
    }
}

/// Parses a document. The error names the line and what was expected, in
/// words someone editing the file by hand can act on.
pub fn parse(text: &str) -> Result<Document, String> {
    let mut p = Parser { src: text.as_bytes(), pos: 0 };
    p.skip();
    let element = p.ident().ok_or_else(|| p.err("an element name such as stoop-theme"))?;
    p.skip();
    p.expect(b'{')?;
    let mut props: Vec<(String, Value)> = Vec::new();
    loop {
        p.skip();
        if p.eat(b'}') {
            break;
        }
        let at = p.line();
        let name = p.ident().ok_or_else(|| p.err("a property name or }"))?;
        if props.iter().any(|(n, _)| *n == name) {
            return Err(format!("Line {at}: {name} is set twice."));
        }
        if props.len() >= MAX_PROPS {
            return Err(format!("Line {at}: too many properties."));
        }
        p.skip();
        p.expect(b':')?;
        p.skip();
        let value = p.value(true)?;
        p.skip();
        p.expect(b';')?;
        props.push((name, value));
    }
    p.skip();
    if p.pos < p.src.len() {
        return Err(p.err("nothing after the closing }"));
    }
    Ok(Document { element, props })
}

/// Writes a document back as markup, one property per line, in a form
/// [`parse`] reads back to the same document. `header` lines are written
/// first as `//` comments.
pub fn write(doc: &Document, header: &[&str]) -> String {
    let mut out = String::new();
    for line in header {
        out.push_str("// ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&doc.element);
    out.push_str(" {\n");
    for (name, value) in &doc.props {
        out.push_str("    ");
        out.push_str(name);
        out.push_str(": ");
        write_value(&mut out, value);
        out.push_str(";\n");
    }
    out.push_str("}\n");
    out
}

/// Appends one value as markup.
fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Str(s) => {
            out.push('"');
            for c in s.chars() {
                // Only the quote and the backslash need escaping; a newline
                // is kept as `\n` so a property stays on one line.
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        Value::Color(c) => out.push_str(&crate::theme::to_hex(*c)),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Ident(s) => out.push_str(s),
        Value::List(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, v);
            }
            out.push(']');
        }
    }
}

/// A cursor over the source bytes. The grammar is ASCII apart from the
/// insides of strings, which are copied through as UTF-8.
struct Parser<'a> {
    /// The whole document.
    src: &'a [u8],
    /// The next byte to read.
    pos: usize,
}

impl Parser<'_> {
    /// The next byte, without consuming it.
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    /// Consumes `b` if it is next.
    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consumes `b`, or fails saying it was expected.
    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.eat(b) {
            Ok(())
        } else {
            Err(self.err(&format!("{}", b as char)))
        }
    }

    /// The 1-based line the cursor is on.
    fn line(&self) -> usize {
        1 + self.src[..self.pos.min(self.src.len())].iter().filter(|&&b| b == b'\n').count()
    }

    /// An error saying `what` was expected where the cursor is.
    fn err(&self, what: &str) -> String {
        let found = match self.peek() {
            None => "the end of the file".to_string(),
            Some(b) if b.is_ascii_graphic() => format!("'{}'", b as char),
            Some(_) => "something else".to_string(),
        };
        format!("Line {}: expected {what}, found {found}.", self.line())
    }

    /// Skips whitespace and both kinds of comment.
    fn skip(&mut self) {
        loop {
            while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
                self.pos += 1;
            }
            if self.src[self.pos..].starts_with(b"//") {
                while self.peek().is_some_and(|b| b != b'\n') {
                    self.pos += 1;
                }
            } else if self.src[self.pos..].starts_with(b"/*") {
                // An unclosed block comment runs to the end; the caller then
                // reports whatever it was still expecting.
                self.pos += 2;
                while self.pos < self.src.len() && !self.src[self.pos..].starts_with(b"*/") {
                    self.pos += 1;
                }
                self.pos = (self.pos + 2).min(self.src.len());
            } else {
                return;
            }
        }
    }

    /// A name: a letter or `_`, then letters, digits, `_` and `-`.
    fn ident(&mut self) -> Option<String> {
        let start = self.pos;
        if !self.peek().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_') {
            return None;
        }
        while self.peek().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            self.pos += 1;
        }
        // The bytes are ASCII, so this cannot fail.
        Some(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
    }

    /// One value; `list_ok` is false inside a list, so lists do not nest.
    fn value(&mut self, list_ok: bool) -> Result<Value, String> {
        match self.peek() {
            Some(b'"') => self.string().map(Value::Str),
            Some(b'#') => {
                let start = self.pos;
                self.pos += 1;
                while self.peek().is_some_and(|b| b.is_ascii_alphanumeric()) {
                    self.pos += 1;
                }
                let raw = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
                parse_hex(raw)
                    .map(Value::Color)
                    .ok_or_else(|| format!("Line {}: {raw} isn't a colour like #8b5cf6.", self.line()))
            }
            Some(b'[') if list_ok => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.skip();
                    if self.eat(b']') {
                        break;
                    }
                    if items.len() >= MAX_LIST {
                        return Err(format!("Line {}: that list is too long.", self.line()));
                    }
                    items.push(self.value(false)?);
                    self.skip();
                    // Items are separated by commas; a trailing one is fine.
                    if !self.eat(b',') {
                        self.skip();
                        self.expect(b']')?;
                        break;
                    }
                }
                Ok(Value::List(items))
            }
            Some(b) if b.is_ascii_digit() || b == b'-' => {
                let start = self.pos;
                self.pos += 1;
                while self.peek().is_some_and(|b| b.is_ascii_digit() || b == b'.') {
                    self.pos += 1;
                }
                let raw = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
                raw.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
                    .map(Value::Number)
                    .ok_or_else(|| format!("Line {}: {raw} isn't a number.", self.line()))
            }
            Some(b) if b.is_ascii_alphabetic() || b == b'_' => {
                let word = self.ident().unwrap_or_default();
                Ok(match word.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => Value::Ident(word),
                })
            }
            _ => Err(self.err("a value")),
        }
    }

    /// A double-quoted string, with `\"`, `\\` and `\n` escapes.
    fn string(&mut self) -> Result<String, String> {
        let line = self.line();
        self.pos += 1;
        let mut bytes = Vec::new();
        loop {
            match self.peek() {
                None => return Err(format!("Line {line}: this string is never closed.")),
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => bytes.push(b'"'),
                        Some(b'\\') => bytes.push(b'\\'),
                        Some(b'n') => bytes.push(b'\n'),
                        _ => return Err(format!("Line {}: unknown escape in a string.", self.line())),
                    }
                    self.pos += 1;
                }
                Some(b) => {
                    bytes.push(b);
                    self.pos += 1;
                }
            }
        }
        String::from_utf8(bytes).map_err(|_| format!("Line {line}: this string isn't valid text."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_kind_of_value() {
        let doc = parse(
            r#"
            // a comment
            stoop-theme { /* and another */
                name: "Mid \"night\"";
                dark: true;
                version: 1;
                font: opendyslexic;
                bg: #0b0b12;
                avatars: [#fff, #000000,];
            }
            "#,
        )
        .unwrap();
        assert_eq!(doc.element, "stoop-theme");
        assert_eq!(doc.text("name"), Some("Mid \"night\""));
        assert_eq!(doc.bool("dark"), Some(true));
        assert_eq!(doc.uint("version"), Some(1));
        assert_eq!(doc.text("font"), Some("opendyslexic"));
        assert_eq!(doc.color("bg"), parse_hex("#0b0b12"));
        assert_eq!(doc.colors("avatars"), Some(vec![Color::WHITE, Color::BLACK]));
    }

    #[test]
    fn writing_then_reading_gives_the_same_document() {
        let doc = Document {
            element: "stoop-theme".into(),
            props: vec![
                ("name".into(), Value::Str("a \\ \"b\"\nc".into())),
                ("n".into(), Value::Number(2.5)),
                ("c".into(), Value::Color(Color::from_rgb8(0x33, 0x66, 0x99))),
                ("l".into(), Value::List(vec![Value::Bool(false), Value::Ident("x".into())])),
            ],
        };
        assert_eq!(parse(&write(&doc, &["hello"])), Ok(doc));
    }

    #[test]
    fn malformed_documents_are_refused() {
        let bad = [
            "",
            "{ a: 1; }",
            "t { a: 1 }",
            "t { a 1; }",
            "t { a: 1; a: 2; }",
            "t { a: #zzzzzz; }",
            "t { a: \"open; }",
            "t { a: [[1]]; }",
            "t { a: 1; } extra",
            "t { a: 1;",
            "t { a: @; }",
        ];
        for text in bad {
            assert!(parse(text).is_err(), "{text:?} should be refused");
        }
    }

    #[test]
    fn errors_name_the_line() {
        let e = parse("t {\n  a: 1;\n  b 2;\n}").unwrap_err();
        assert!(e.starts_with("Line 3:"), "{e}");
    }
}
