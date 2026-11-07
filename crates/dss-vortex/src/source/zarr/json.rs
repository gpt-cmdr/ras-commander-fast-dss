//! JSON for Zarr metadata: RFC 8259, plus the bare `NaN`, `Infinity` and `-Infinity` that Python's
//! `json.dumps` writes and `zarr-python` reads back.
//!
//! A parser of our own, because the input is untrusted and every bound is stated here: the text's
//! length is the caller's, nesting stops at [`MAX_DEPTH`], and every container grows by a fallible
//! reservation. A value's count cannot pass the text's length, so no loop runs past its input.

/// Deepest nesting. Zarr metadata nests four deep (`metadata`, key, `compressor`, a list).
pub(super) const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Json {
    Null,
    Bool(bool),
    /// A number with no fraction or exponent that fits `i64`.
    Int(i64),
    /// Any other number, and the three bare words.
    Float(f64),
    Text(String),
    List(Vec<Json>),
    /// In the text's order. A repeated key is refused when parsed.
    Map(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Json::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Json]> {
        match self {
            Json::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Map(m) => Some(m),
            _ => None,
        }
    }

    /// A number, as a double. An integer over 2^53 in size has no exact double, and is `None`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Int(i) if i.unsigned_abs() <= (1 << 53) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(i) => u64::try_from(*i).ok(),
            _ => None,
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

type Parsed<T> = Result<T, String>;

pub(super) fn parse(bytes: &[u8]) -> Parsed<Json> {
    // A byte order mark is not JSON, but files written on Windows have one.
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut p = Parser { bytes, at: 0 };
    let value = p.value(0)?;
    p.space();
    if p.at != bytes.len() {
        return Err(p.fail("text after the value"));
    }
    Ok(value)
}

fn no_room() -> String {
    "could not allocate its values".to_string()
}

impl Parser<'_> {
    fn fail(&self, why: &str) -> String {
        format!("{why} at byte {}", self.at)
    }

    fn space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn word(&mut self, word: &str) -> bool {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Parsed<Json> {
        if depth > MAX_DEPTH {
            return Err(self.fail("nesting over the limit"));
        }
        self.space();
        match self.bytes.get(self.at) {
            None => Err(self.fail("the text ends")),
            Some(b'{') => self.map(depth),
            Some(b'[') => self.list(depth),
            Some(b'"') => self.text().map(Json::Text),
            Some(b't') if self.word("true") => Ok(Json::Bool(true)),
            Some(b'f') if self.word("false") => Ok(Json::Bool(false)),
            Some(b'n') if self.word("null") => Ok(Json::Null),
            Some(b'N') if self.word("NaN") => Ok(Json::Float(f64::NAN)),
            Some(b'I') if self.word("Infinity") => Ok(Json::Float(f64::INFINITY)),
            Some(b'-') if self.word("-Infinity") => Ok(Json::Float(f64::NEG_INFINITY)),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.fail("not a value")),
        }
    }

    fn map(&mut self, depth: usize) -> Parsed<Json> {
        self.at += 1;
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.space();
        if self.bytes.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Ok(Json::Map(entries));
        }
        loop {
            self.space();
            if self.bytes.get(self.at) != Some(&b'"') {
                return Err(self.fail("a key must be text"));
            }
            let key = self.text()?;
            self.space();
            if self.bytes.get(self.at) != Some(&b':') {
                return Err(self.fail("no colon after a key"));
            }
            self.at += 1;
            let value = self.value(depth + 1)?;
            // Readers disagree on which of two wins, so neither does.
            if entries.iter().any(|(k, _)| *k == key) {
                return Err(self.fail("a key appears twice"));
            }
            entries.try_reserve(1).map_err(|_| no_room())?;
            entries.push((key, value));
            self.space();
            match self.bytes.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Json::Map(entries));
                }
                _ => return Err(self.fail("no comma or brace after a member")),
            }
        }
    }

    fn list(&mut self, depth: usize) -> Parsed<Json> {
        self.at += 1;
        let mut items: Vec<Json> = Vec::new();
        self.space();
        if self.bytes.get(self.at) == Some(&b']') {
            self.at += 1;
            return Ok(Json::List(items));
        }
        loop {
            let value = self.value(depth + 1)?;
            items.try_reserve(1).map_err(|_| no_room())?;
            items.push(value);
            self.space();
            match self.bytes.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Json::List(items));
                }
                _ => return Err(self.fail("no comma or bracket after an item")),
            }
        }
    }

    fn hex4(&mut self) -> Parsed<u32> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| self.fail("a \\u escape needs four hex digits"))?;
        let value = u32::from_str_radix(digits, 16).map_err(|_| self.fail("a \\u escape"))?;
        self.at += 4;
        Ok(value)
    }

    fn text(&mut self) -> Parsed<String> {
        self.at += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let start = self.at;
            while let Some(b) = self.bytes.get(self.at) {
                if *b == b'"' || *b == b'\\' || *b < 0x20 {
                    break;
                }
                self.at += 1;
            }
            out.try_reserve(self.at - start + 4)
                .map_err(|_| no_room())?;
            out.extend_from_slice(&self.bytes[start..self.at]);
            match self.bytes.get(self.at) {
                Some(b'"') => {
                    self.at += 1;
                    return String::from_utf8(out).map_err(|_| self.fail("text is not UTF-8"));
                }
                Some(b'\\') => {
                    self.at += 1;
                    let escape = *self
                        .bytes
                        .get(self.at)
                        .ok_or_else(|| self.fail("the text ends in an escape"))?;
                    self.at += 1;
                    let c = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let high = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&high) {
                                if !self.word("\\u") {
                                    return Err(self.fail("half a surrogate pair"));
                                }
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err(self.fail("half a surrogate pair"));
                                }
                                0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                            } else {
                                high
                            };
                            char::from_u32(code)
                                .ok_or_else(|| self.fail("half a surrogate pair"))?
                        }
                        _ => return Err(self.fail("an unknown escape")),
                    };
                    let mut utf8 = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
                }
                Some(_) => return Err(self.fail("a control character in text")),
                None => return Err(self.fail("the text ends inside a string")),
            }
        }
    }

    fn number(&mut self) -> Parsed<Json> {
        let start = self.at;
        if self.bytes.get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        let digits = |p: &mut Self| {
            let from = p.at;
            while matches!(p.bytes.get(p.at), Some(b'0'..=b'9')) {
                p.at += 1;
            }
            p.at - from
        };
        let whole_from = self.at;
        let whole = digits(self);
        if whole == 0 || (whole > 1 && self.bytes[whole_from] == b'0') {
            return Err(self.fail("not a number"));
        }
        let mut integer = true;
        if self.bytes.get(self.at) == Some(&b'.') {
            self.at += 1;
            integer = false;
            if digits(self) == 0 {
                return Err(self.fail("no digits after a point"));
            }
        }
        if matches!(self.bytes.get(self.at), Some(b'e' | b'E')) {
            self.at += 1;
            integer = false;
            if matches!(self.bytes.get(self.at), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if digits(self) == 0 {
                return Err(self.fail("no digits in an exponent"));
            }
        }
        // Digits, a sign, a point and an exponent letter: ASCII.
        let text = std::str::from_utf8(&self.bytes[start..self.at])
            .map_err(|_| self.fail("not a number"))?;
        if integer {
            if let Ok(value) = text.parse::<i64>() {
                return Ok(Json::Int(value));
            }
        }
        // Rust's parse is correctly rounded, as Python's `float` is.
        text.parse::<f64>()
            .map(Json::Float)
            .map_err(|_| self.fail("not a number"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse() {
        let v = parse(br#"{"a": [1, -2.5, 1e3, "x\u00e9\ud83d\ude00\n"], "b": null, "c": true}"#)
            .unwrap();
        assert_eq!(
            v.get("a").unwrap().as_list().unwrap(),
            [
                Json::Int(1),
                Json::Float(-2.5),
                Json::Float(1000.0),
                Json::Text("x\u{e9}\u{1F600}\n".into())
            ]
        );
        assert_eq!(v.get("b"), Some(&Json::Null));
        assert_eq!(v.get("c"), Some(&Json::Bool(true)));
        assert_eq!(v.get("d"), None);
    }

    #[test]
    fn python_bare_words_parse() {
        let v = parse(b"[NaN, Infinity, -Infinity]").unwrap();
        let l = v.as_list().unwrap();
        assert!(l[0].as_f64().unwrap().is_nan());
        assert_eq!(l[1].as_f64(), Some(f64::INFINITY));
        assert_eq!(l[2].as_f64(), Some(f64::NEG_INFINITY));
    }

    #[test]
    fn numbers_keep_their_kind() {
        assert_eq!(parse(b"-32767").unwrap(), Json::Int(-32767));
        assert_eq!(parse(b"-32767.0").unwrap(), Json::Float(-32767.0));
        assert_eq!(
            parse(b"0.10000000149011612").unwrap(),
            Json::Float(f64::from(0.1_f32))
        );
        // Over i64: a double, as Python reads it into an int and numpy then refuses.
        assert_eq!(
            parse(b"18446744073709551616").unwrap(),
            Json::Float(18446744073709551616.0)
        );
        assert_eq!(Json::Int(1 << 60).as_f64(), None);
    }

    #[test]
    fn malformed_text_is_an_error() {
        for bad in [
            &b""[..],
            b"{",
            b"[1,]",
            b"{\"a\":1,}",
            b"{\"a\" 1}",
            b"{a:1}",
            b"01",
            b"1.",
            b"1e",
            b"\"abc",
            b"\"\\x\"",
            b"\"\\ud800\"",
            b"\"\\ud800\\u0041\"",
            b"\"a\nb\"",
            b"nul",
            b"1 2",
            b"{\"a\":1,\"a\":2}",
            b"\xff",
            b"\"\xff\"",
            b"-",
            b"-Inf",
        ] {
            assert!(parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn nesting_stops_at_the_limit() {
        let deep = |n: usize| [vec![b'['; n], vec![b']'; n]].concat();
        assert!(parse(&deep(MAX_DEPTH)).is_ok());
        assert!(parse(&deep(MAX_DEPTH + 2)).is_err());
        // Far past any stack a recursive parser without a limit would have.
        assert!(parse(&vec![b'['; 1 << 20]).is_err());
    }
}
