//! JSON as far as tb-panel needs it: a tree, read from a panel file, a question file or a
//! line of the app, and written back on one line. Objects keep the order of their members.
//! A key given twice is refused: in a file that decides what runs as root, the reader must
//! not have to know which of the two wins.

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// Nesting deeper than this is refused. A panel file is four deep, a line of the app three.
const MAX_DEPTH: usize = 64;

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut read = Read { text: text.as_bytes(), at: 0 };
        let value = read.value(0)?;
        read.space();
        if read.at < read.text.len() {
            return read.fail("text after the value");
        }
        Ok(value)
    }

    /// An object from its members in order.
    pub fn obj<const N: usize>(members: [(&str, Json); N]) -> Json {
        Json::Obj(members.into_iter().map(|(key, value)| (key.to_string(), value)).collect())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Sets a member of an object, in place when it is there.
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Obj(members) = self {
            match members.iter_mut().find(|(k, _)| k == key) {
                Some(member) => member.1 = value,
                None => members.push((key.to_string(), value)),
            }
        }
    }

    pub fn remove(&mut self, key: &str) {
        if let Json::Obj(members) = self {
            members.retain(|(k, _)| k != key);
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(value) => Some(*value),
            _ => None,
        }
    }

    /// A whole number of at most 2^53, as counters and revs are.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) if *n >= 0.0 && n.fract() == 0.0 && *n <= 9007199254740992.0 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(members) => Some(members),
            _ => None,
        }
    }
}

impl From<&str> for Json {
    fn from(text: &str) -> Json {
        Json::Str(text.to_string())
    }
}

impl From<String> for Json {
    fn from(text: String) -> Json {
        Json::Str(text)
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Json {
        Json::Bool(value)
    }
}

impl From<f64> for Json {
    fn from(value: f64) -> Json {
        Json::Num(value)
    }
}

impl From<u64> for Json {
    fn from(value: u64) -> Json {
        Json::Num(value as f64)
    }
}

impl From<i32> for Json {
    fn from(value: i32) -> Json {
        Json::Num(value as f64)
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(value: Option<T>) -> Json {
        value.map_or(Json::Null, Into::into)
    }
}

impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(items: Vec<T>) -> Json {
        Json::Arr(items.into_iter().map(Into::into).collect())
    }
}

fn write_str(f: &mut fmt::Formatter, text: &str) -> fmt::Result {
    f.write_str("\"")?;
    for c in text.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
            c => write!(f, "{c}")?,
        }
    }
    f.write_str("\"")
}

/// On one line, without spaces.
impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(value) => write!(f, "{value}"),
            // a float that is not a number has no JSON form
            Json::Num(n) if !n.is_finite() => f.write_str("null"),
            Json::Num(n) => write!(f, "{n}"),
            Json::Str(text) => write_str(f, text),
            Json::Arr(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Json::Obj(members) => {
                f.write_str("{")?;
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write_str(f, key)?;
                    write!(f, ":{value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

struct Read<'a> {
    text: &'a [u8],
    at: usize,
}

impl Read<'_> {
    fn fail<T>(&self, what: &str) -> Result<T, String> {
        let before = &self.text[..self.at.min(self.text.len())];
        let line = before.iter().filter(|b| **b == b'\n').count() + 1;
        let column = before.iter().rev().take_while(|b| **b != b'\n').count() + 1;
        Err(format!("{what} (line {line} column {column})"))
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        self.at += found as usize;
        found
    }

    fn word(&mut self, word: &str) -> bool {
        let found = self.text[self.at..].starts_with(word.as_bytes());
        self.at += if found { word.len() } else { 0 };
        found
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn digits(&mut self) -> bool {
        let from = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at > from
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return self.fail("nested too deep");
        }
        self.space();
        match self.peek() {
            Some(b'{') => {
                self.at += 1;
                let mut members: Vec<(String, Json)> = Vec::new();
                self.space();
                if self.eat(b'}') {
                    return Ok(Json::Obj(members));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    if members.iter().any(|(k, _)| *k == key) {
                        return self.fail(&format!("the key {key:?} is given twice"));
                    }
                    self.space();
                    if !self.eat(b':') {
                        return self.fail("':' is expected");
                    }
                    members.push((key, self.value(depth + 1)?));
                    self.space();
                    if self.eat(b'}') {
                        return Ok(Json::Obj(members));
                    }
                    if !self.eat(b',') {
                        return self.fail("',' or '}' is expected");
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.eat(b']') {
                    return Ok(Json::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.space();
                    if self.eat(b']') {
                        return Ok(Json::Arr(items));
                    }
                    if !self.eat(b',') {
                        return self.fail("',' or ']' is expected");
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') if self.word("true") => Ok(Json::Bool(true)),
            Some(b'f') if self.word("false") => Ok(Json::Bool(false)),
            Some(b'n') if self.word("null") => Ok(Json::Null),
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.at;
        self.eat(b'-');
        if !self.eat(b'0') && !self.digits() {
            return self.fail("a value is expected");
        }
        if self.eat(b'.') && !self.digits() {
            return self.fail("digits are expected after the point");
        }
        if self.eat(b'e') || self.eat(b'E') {
            let _ = self.eat(b'+') || self.eat(b'-');
            if !self.digits() {
                return self.fail("digits are expected in the exponent");
            }
        }
        // the slice is ASCII by construction
        match std::str::from_utf8(&self.text[start..self.at]).ok().and_then(|n| n.parse().ok()) {
            Some(n) => Ok(Json::Num(n)),
            None => self.fail("not a number"),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self.text.get(self.at..self.at + 4).and_then(|d| std::str::from_utf8(d).ok());
        match digits.and_then(|d| u32::from_str_radix(d, 16).ok()) {
            Some(code) => {
                self.at += 4;
                Ok(code)
            }
            None => self.fail("\\u takes four hex digits"),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if !self.eat(b'"') {
            return self.fail("a string is expected");
        }
        let mut out = Vec::new();
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.at += 1;
                    // the input is UTF-8 and escapes add only whole characters
                    return String::from_utf8(out).or_else(|_| self.fail("a string is not UTF-8"));
                }
                Some(b'\\') => {
                    self.at += 1;
                    let escaped = self.peek();
                    self.at += 1;
                    let c = match escaped {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            let mut code = self.hex4()?;
                            // a pair of surrogates is one character
                            if (0xD800..0xDC00).contains(&code) && self.text[self.at..].starts_with(b"\\u") {
                                let back = self.at;
                                self.at += 2;
                                let low = self.hex4()?;
                                if (0xDC00..0xE000).contains(&low) {
                                    code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                } else {
                                    self.at = back;
                                }
                            }
                            char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER)
                        }
                        _ => {
                            self.at -= 1;
                            return self.fail("an unknown escape in a string");
                        }
                    };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                Some(byte) if byte >= 0x20 => {
                    out.push(byte);
                    self.at += 1;
                }
                Some(_) => return self.fail("a control character in a string"),
                None => return self.fail("a string does not end"),
            }
        }
    }
}
