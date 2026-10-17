//! Test-only helpers: a tiny JSON reader (no serde, even in dev-deps) and vector loading.
#![allow(dead_code)]

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> &Json {
        match self {
            Json::Obj(kv) => kv
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
                .unwrap_or_else(|| panic!("missing key {key:?}")),
            _ => panic!("not an object looking up {key:?}"),
        }
    }
    pub fn opt(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            other => panic!("not an array: {other:?}"),
        }
    }
    pub fn obj(&self) -> &[(String, Json)] {
        match self {
            Json::Obj(o) => o,
            other => panic!("not an object: {other:?}"),
        }
    }
    pub fn f64(&self) -> f64 {
        match self {
            Json::Num(n) => *n,
            other => panic!("not a number: {other:?}"),
        }
    }
    pub fn u64(&self) -> u64 {
        self.f64() as u64
    }
    pub fn str(&self) -> &str {
        match self {
            Json::Str(s) => s,
            other => panic!("not a string: {other:?}"),
        }
    }
    pub fn bool(&self) -> bool {
        match self {
            Json::Bool(b) => *b,
            other => panic!("not a bool: {other:?}"),
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
    /// `[[r,g,b], ...]`
    pub fn rgb_list(&self) -> Vec<[u8; 3]> {
        self.arr()
            .iter()
            .map(|c| {
                let c = c.arr();
                [c[0].u64() as u8, c[1].u64() as u8, c[2].u64() as u8]
            })
            .collect()
    }
}

pub fn parse(text: &str) -> Json {
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
    };
    let v = p.value();
    p.ws();
    assert_eq!(p.i, p.b.len(), "trailing data in JSON");
    v
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) {
        self.ws();
        assert_eq!(self.b[self.i], c, "expected {:?} at {}", c as char, self.i);
        self.i += 1;
    }
    fn lit(&mut self, s: &str, v: Json) -> Json {
        assert!(self.b[self.i..].starts_with(s.as_bytes()));
        self.i += s.len();
        v
    }
    fn value(&mut self) -> Json {
        self.ws();
        match self.b[self.i] {
            b'{' => {
                self.i += 1;
                let mut kv = Vec::new();
                self.ws();
                if self.b[self.i] == b'}' {
                    self.i += 1;
                    return Json::Obj(kv);
                }
                loop {
                    self.ws();
                    let k = self.string();
                    self.eat(b':');
                    kv.push((k, self.value()));
                    self.ws();
                    match self.b[self.i] {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            return Json::Obj(kv);
                        }
                        c => panic!("bad object char {:?}", c as char),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b[self.i] == b']' {
                    self.i += 1;
                    return Json::Arr(a);
                }
                loop {
                    a.push(self.value());
                    self.ws();
                    match self.b[self.i] {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Json::Arr(a);
                        }
                        c => panic!("bad array char {:?}", c as char),
                    }
                }
            }
            b'"' => Json::Str(self.string()),
            b't' => self.lit("true", Json::Bool(true)),
            b'f' => self.lit("false", Json::Bool(false)),
            b'n' => self.lit("null", Json::Null),
            _ => {
                let s = self.i;
                while self.i < self.b.len()
                    && matches!(
                        self.b[self.i],
                        b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
                    )
                {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[s..self.i]).unwrap();
                Json::Num(t.parse().unwrap_or_else(|_| panic!("bad number {t:?}")))
            }
        }
    }
    fn string(&mut self) -> String {
        assert_eq!(self.b[self.i], b'"');
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = self.b[self.i];
            self.i += 1;
            match c {
                b'"' => return out,
                b'\\' => {
                    let e = self.b[self.i];
                    self.i += 1;
                    match e {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let h = std::str::from_utf8(&self.b[self.i..self.i + 4]).unwrap();
                            self.i += 4;
                            out.push(
                                char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap_or('?'),
                            );
                        }
                        other => out.push(other as char),
                    }
                }
                _ => {
                    // Copy a run of UTF-8 bytes verbatim.
                    let s = self.i - 1;
                    while self.i < self.b.len() && self.b[self.i] != b'"' && self.b[self.i] != b'\\'
                    {
                        self.i += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.b[s..self.i]).unwrap());
                }
            }
        }
    }
}

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn vector(name: &str) -> Json {
    let p = root().join("tests/vectors").join(name);
    parse(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
}

/// `gen_vectors.synthetic_frame`: r=(7x+3y)%256 g=(2x+5y)%256 b=(x*x+y)%256.
pub fn synthetic_frame(h: usize, w: usize) -> Vec<u8> {
    let mut f = Vec::with_capacity(h * w * 3);
    for y in 0..h {
        for x in 0..w {
            f.push(((7 * x + 3 * y) % 256) as u8);
            f.push(((2 * x + 5 * y) % 256) as u8);
            f.push(((x * x + y) % 256) as u8);
        }
    }
    f
}
