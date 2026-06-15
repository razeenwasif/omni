//! A tiny JSON reader — just enough to accept page payloads from Flux on
//! `POST /ingest` (`{url,title,text}` or an array of them). Hand-rolled to keep
//! the no-dependency ethos; it parses objects/arrays and fully unescapes strings,
//! and skips any non-string field values it doesn't care about.

/// One ingested page from a JSON payload.
pub struct Page {
    pub url: String,
    pub title: String,
    pub text: String,
    pub published: String,
}

/// Parse a JSON object `{...}` or array of objects `[{...}, ...]` into pages.
/// Returns `None` if the input isn't JSON (so the caller can fall back to the
/// doc-store text format).
pub fn parse_pages(s: &str) -> Option<Vec<Page>> {
    let mut p = Parser {
        b: s.as_bytes(),
        i: 0,
    };
    p.ws();
    let objects = match p.peek()? {
        b'[' => p.array()?,
        b'{' => vec![p.object()?],
        _ => return None,
    };
    Some(
        objects
            .into_iter()
            .map(|mut o| Page {
                url: o.remove("url").unwrap_or_default(),
                title: o.remove("title").unwrap_or_default(),
                text: o.remove("text").unwrap_or_default(),
                published: o.remove("published").unwrap_or_default(),
            })
            .collect(),
    )
}

use std::collections::HashMap;

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }
    fn expect(&mut self, c: u8) -> Option<()> {
        self.ws();
        (self.bump()? == c).then_some(())
    }

    /// Parse `[ obj, obj, ... ]`, keeping only object elements.
    fn array(&mut self) -> Option<Vec<HashMap<String, String>>> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        self.ws();
        if self.peek()? == b']' {
            self.i += 1;
            return Some(out);
        }
        loop {
            self.ws();
            if self.peek()? == b'{' {
                out.push(self.object()?);
            } else {
                self.skip_value()?;
            }
            self.ws();
            match self.bump()? {
                b',' => continue,
                b']' => break,
                _ => return None,
            }
        }
        Some(out)
    }

    /// Parse `{ "k": value, ... }`, capturing only string-valued fields.
    fn object(&mut self) -> Option<HashMap<String, String>> {
        self.expect(b'{')?;
        let mut map = HashMap::new();
        self.ws();
        if self.peek()? == b'}' {
            self.i += 1;
            return Some(map);
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.expect(b':')?;
            self.ws();
            if self.peek()? == b'"' {
                map.insert(key, self.string()?);
            } else {
                self.skip_value()?;
            }
            self.ws();
            match self.bump()? {
                b',' => continue,
                b'}' => break,
                _ => return None,
            }
        }
        Some(map)
    }

    /// Parse a JSON string literal, resolving escapes. Accumulates raw bytes so
    /// multi-byte UTF-8 passes through unchanged; `\u` escapes are re-encoded.
    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            match self.bump()? {
                b'"' => break,
                b'\\' => match self.bump()? {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'u' => {
                        let cp = self.hex4()?;
                        // Re-assemble a surrogate pair for astral characters.
                        let c = if (0xD800..=0xDBFF).contains(&cp) {
                            if self.bump()? != b'\\' || self.bump()? != b'u' {
                                return None;
                            }
                            let lo = self.hex4()?;
                            0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00)
                        } else {
                            cp
                        };
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(char::from_u32(c)?.encode_utf8(&mut buf).as_bytes());
                    }
                    _ => return None,
                },
                c => out.push(c), // raw byte — valid UTF-8 sequences pass through
            }
        }
        String::from_utf8(out).ok()
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = (self.bump()? as char).to_digit(16)?;
            v = v * 16 + d;
        }
        Some(v)
    }

    /// Parse and discard any JSON value (used for fields we don't capture).
    fn skip_value(&mut self) -> Option<()> {
        self.ws();
        match self.peek()? {
            b'"' => {
                self.string()?;
            }
            b'{' => {
                self.object()?;
            }
            b'[' => {
                self.array_skip()?;
            }
            _ => {
                // number / true / false / null — consume the literal run.
                while matches!(self.peek(), Some(c) if !matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'))
                {
                    self.i += 1;
                }
            }
        }
        Some(())
    }

    fn array_skip(&mut self) -> Option<()> {
        self.expect(b'[')?;
        self.ws();
        if self.peek()? == b']' {
            self.i += 1;
            return Some(());
        }
        loop {
            self.skip_value()?;
            self.ws();
            match self.bump()? {
                b',' => continue,
                b']' => break,
                _ => return None,
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_object() {
        let pages =
            parse_pages(r#"{"url":"http://x/a","title":"A","text":"hello world"}"#).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].url, "http://x/a");
        assert_eq!(pages[0].title, "A");
        assert_eq!(pages[0].text, "hello world");
    }

    #[test]
    fn parses_array_and_escapes_and_skips_other_fields() {
        let body = r#"[
          {"url":"u1","title":"T \"quoted\"","text":"line1\nline2","status":200,"ok":true},
          {"url":"u2","title":"Café — ☕","text":"unicode é and 🚀","nested":{"a":[1,2]}}
        ]"#;
        let pages = parse_pages(body).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].title, "T \"quoted\"");
        assert_eq!(pages[0].text, "line1\nline2");
        assert_eq!(pages[1].title, "Café — ☕");
        assert_eq!(pages[1].text, "unicode é and 🚀");
    }

    #[test]
    fn non_json_returns_none() {
        assert!(parse_pages("url: http://x\ntitle: T\n\nbody").is_none());
        assert!(parse_pages("").is_none());
    }
}
