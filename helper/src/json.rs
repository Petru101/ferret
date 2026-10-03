// Just enough JSON to read launchers' caches (the helper has no dependencies).

pub enum Value {
    /// null, true, false and numbers (nothing reads them).
    Other,
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn arr(&self) -> &[Value] {
        match self {
            Value::Arr(items) => items,
            _ => &[],
        }
    }
}

pub fn parse(text: &str) -> Option<Value> {
    let mut p = Parser { b: text.as_bytes(), i: 0 };
    let v = p.value()?;
    p.ws();
    (p.i == p.b.len()).then_some(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.b.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        let found = self.b.get(self.i) == Some(&c);
        self.i += found as usize;
        found
    }

    fn value(&mut self) -> Option<Value> {
        self.ws();
        match *self.b.get(self.i)? {
            b'{' => {
                self.i += 1;
                let mut fields = Vec::new();
                if !self.eat(b'}') {
                    loop {
                        self.ws();
                        let k = self.string()?;
                        self.eat(b':').then_some(())?;
                        fields.push((k, self.value()?));
                        if self.eat(b'}') {
                            break;
                        }
                        self.eat(b',').then_some(())?;
                    }
                }
                Some(Value::Obj(fields))
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                if !self.eat(b']') {
                    loop {
                        items.push(self.value()?);
                        if self.eat(b']') {
                            break;
                        }
                        self.eat(b',').then_some(())?;
                    }
                }
                Some(Value::Arr(items))
            }
            b'"' => self.string().map(Value::Str),
            b't' => self.word("true"),
            b'f' => self.word("false"),
            b'n' => self.word("null"),
            _ => {
                let start = self.i;
                while self.b.get(self.i).is_some_and(|c| matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.i += 1;
                }
                std::str::from_utf8(&self.b[start..self.i]).ok()?.parse::<f64>().ok().map(|_| Value::Other)
            }
        }
    }

    fn word(&mut self, w: &str) -> Option<Value> {
        self.b[self.i..].starts_with(w.as_bytes()).then(|| self.i += w.len())?;
        Some(Value::Other)
    }

    fn string(&mut self) -> Option<String> {
        (self.b.get(self.i) == Some(&b'"')).then_some(())?;
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    let ch = match e {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            let mut u = self.hex4()?;
                            // A pair of UTF-16 halves.
                            if (0xD800..0xDC00).contains(&u) && self.b[self.i..].starts_with(b"\\u") {
                                self.i += 2;
                                let low = self.hex4()?;
                                u = 0x10000 + ((u - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                            }
                            char::from_u32(u).unwrap_or('\u{FFFD}')
                        }
                        other => other as char,
                    };
                    let mut buf = [0; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                _ => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let s = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(s, 16).ok()
    }
}
