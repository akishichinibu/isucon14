//! Bounded JSON scanner/writer for the ISURIDE OpenAPI bodies.

use crate::Error;

pub struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    pub fn new(s: &'a [u8]) -> Self {
        Self { s, i: 0 }
    }

    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.s.get(self.i).copied()?;
        self.i += 1;
        Some(b)
    }

    fn eat(&mut self, b: u8) -> Result<(), Error> {
        self.skip_ws();
        if self.bump() == Some(b) {
            Ok(())
        } else {
            Err(Error::BadRequest("invalid json"))
        }
    }

    pub fn parse_object<F>(&mut self, mut field: F) -> Result<(), Error>
    where
        F: FnMut(&str, &mut Parser<'a>) -> Result<(), Error>,
    {
        self.eat(b'{')?;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(());
        }
        loop {
            let key = self.parse_string()?;
            self.eat(b':')?;
            field(&key, self)?;
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => return Ok(()),
                _ => return Err(Error::BadRequest("invalid json")),
            }
        }
    }

    pub fn parse_string(&mut self) -> Result<String, Error> {
        self.skip_ws();
        if self.bump() != Some(b'"') {
            return Err(Error::BadRequest("invalid json string"));
        }
        let mut out = String::new();
        loop {
            let b = self.bump().ok_or(Error::BadRequest("unterminated string"))?;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.bump().ok_or(Error::BadRequest("bad escape"))?;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e as char),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut cp = 0u32;
                            for _ in 0..4 {
                                let h = self.bump().ok_or(Error::BadRequest("bad unicode"))?;
                                cp = (cp << 4)
                                    | match h {
                                        b'0'..=b'9' => (h - b'0') as u32,
                                        b'a'..=b'f' => (h - b'a' + 10) as u32,
                                        b'A'..=b'F' => (h - b'A' + 10) as u32,
                                        _ => return Err(Error::BadRequest("bad unicode")),
                                    };
                            }
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(Error::BadRequest("bad escape")),
                    }
                }
                _ => {
                    // UTF-8: consume continuation if this is a lead byte.
                    if b < 0x80 {
                        out.push(b as char);
                    } else {
                        self.i -= 1;
                        let rest = &self.s[self.i..];
                        let s = std::str::from_utf8(rest)
                            .map_err(|_| Error::BadRequest("invalid utf8"))?;
                        let ch = s.chars().next().ok_or(Error::BadRequest("invalid utf8"))?;
                        let n = ch.len_utf8();
                        out.push(ch);
                        self.i += n;
                    }
                }
            }
        }
    }

    pub fn parse_i64(&mut self) -> Result<i64, Error> {
        self.skip_ws();
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        if self.i >= self.s.len() || !self.s[self.i].is_ascii_digit() {
            return Err(Error::BadRequest("invalid number"));
        }
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or(Error::BadRequest("invalid number"))
    }

    pub fn parse_i32(&mut self) -> Result<i32, Error> {
        self.parse_i64()?
            .try_into()
            .map_err(|_| Error::BadRequest("invalid number"))
    }

    pub fn parse_bool(&mut self) -> Result<bool, Error> {
        self.skip_ws();
        if self.s[self.i..].starts_with(b"true") {
            self.i += 4;
            Ok(true)
        } else if self.s[self.i..].starts_with(b"false") {
            self.i += 5;
            Ok(false)
        } else {
            Err(Error::BadRequest("invalid bool"))
        }
    }

    pub fn skip_value(&mut self) -> Result<(), Error> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => {
                self.parse_string()?;
                Ok(())
            }
            Some(b'{') => self.parse_object(|_, p| p.skip_value()),
            Some(b'[') => {
                self.eat(b'[')?;
                self.skip_ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.skip_value()?;
                    self.skip_ws();
                    match self.bump() {
                        Some(b',') => continue,
                        Some(b']') => return Ok(()),
                        _ => return Err(Error::BadRequest("invalid json")),
                    }
                }
            }
            Some(b't') | Some(b'f') => {
                self.parse_bool()?;
                Ok(())
            }
            Some(b'n') => {
                if self.s[self.i..].starts_with(b"null") {
                    self.i += 4;
                    Ok(())
                } else {
                    Err(Error::BadRequest("invalid json"))
                }
            }
            Some(b'-') | Some(b'0'..=b'9') => {
                self.parse_i64()?;
                Ok(())
            }
            _ => Err(Error::BadRequest("invalid json")),
        }
    }
}

pub fn parse_coordinate(body: &[u8]) -> Result<(i32, i32), Error> {
    let mut lat = None;
    let mut lng = None;
    let mut p = Parser::new(body);
    p.parse_object(|k, p| match k {
        "latitude" => {
            lat = Some(p.parse_i32()?);
            Ok(())
        }
        "longitude" => {
            lng = Some(p.parse_i32()?);
            Ok(())
        }
        _ => p.skip_value(),
    })?;
    Ok((
        lat.ok_or(Error::BadRequest("latitude required"))?,
        lng.ok_or(Error::BadRequest("longitude required"))?,
    ))
}

pub fn parse_two_coordinates(body: &[u8]) -> Result<(i32, i32, i32, i32), Error> {
    let mut pickup = None;
    let mut dest = None;
    let mut p = Parser::new(body);
    p.parse_object(|k, p| match k {
        "pickup_coordinate" => {
            pickup = Some(parse_coordinate_in(p)?);
            Ok(())
        }
        "destination_coordinate" => {
            dest = Some(parse_coordinate_in(p)?);
            Ok(())
        }
        _ => p.skip_value(),
    })?;
    let (plat, plng) = pickup.ok_or(Error::BadRequest("pickup_coordinate required"))?;
    let (dlat, dlng) = dest.ok_or(Error::BadRequest("destination_coordinate required"))?;
    Ok((plat, plng, dlat, dlng))
}

fn parse_coordinate_in(p: &mut Parser<'_>) -> Result<(i32, i32), Error> {
    let mut lat = None;
    let mut lng = None;
    p.parse_object(|k, p| match k {
        "latitude" => {
            lat = Some(p.parse_i32()?);
            Ok(())
        }
        "longitude" => {
            lng = Some(p.parse_i32()?);
            Ok(())
        }
        _ => p.skip_value(),
    })?;
    Ok((
        lat.ok_or(Error::BadRequest("latitude required"))?,
        lng.ok_or(Error::BadRequest("longitude required"))?,
    ))
}

pub fn parse_named_i32(body: &[u8], key: &str) -> Result<i32, Error> {
    let mut v = None;
    let mut p = Parser::new(body);
    p.parse_object(|k, p| {
        if k == key {
            v = Some(p.parse_i32()?);
            Ok(())
        } else {
            p.skip_value()
        }
    })?;
    v.ok_or(Error::BadRequest("missing field"))
}

pub fn parse_named_bool(body: &[u8], key: &str) -> Result<bool, Error> {
    let mut v = None;
    let mut p = Parser::new(body);
    p.parse_object(|k, p| {
        if k == key {
            v = Some(p.parse_bool()?);
            Ok(())
        } else {
            p.skip_value()
        }
    })?;
    v.ok_or(Error::BadRequest("missing field"))
}

pub fn parse_named_string(body: &[u8], key: &str) -> Result<String, Error> {
    let mut v = None;
    let mut p = Parser::new(body);
    p.parse_object(|k, p| {
        if k == key {
            v = Some(p.parse_string()?);
            Ok(())
        } else {
            p.skip_value()
        }
    })?;
    v.ok_or(Error::BadRequest("missing field"))
}

pub struct UserReg {
    pub username: String,
    pub firstname: String,
    pub lastname: String,
    pub date_of_birth: String,
    pub invitation_code: Option<String>,
}

pub fn parse_user_reg(body: &[u8]) -> Result<UserReg, Error> {
    let mut username = None;
    let mut firstname = None;
    let mut lastname = None;
    let mut date_of_birth = None;
    let mut invitation_code = None;
    let mut p = Parser::new(body);
    p.parse_object(|k, p| match k {
        "username" => {
            username = Some(p.parse_string()?);
            Ok(())
        }
        "firstname" => {
            firstname = Some(p.parse_string()?);
            Ok(())
        }
        "lastname" => {
            lastname = Some(p.parse_string()?);
            Ok(())
        }
        "date_of_birth" => {
            date_of_birth = Some(p.parse_string()?);
            Ok(())
        }
        "invitation_code" => {
            invitation_code = Some(p.parse_string()?);
            Ok(())
        }
        _ => p.skip_value(),
    })?;
    Ok(UserReg {
        username: username.ok_or(Error::BadRequest("username required"))?,
        firstname: firstname.ok_or(Error::BadRequest("firstname required"))?,
        lastname: lastname.ok_or(Error::BadRequest("lastname required"))?,
        date_of_birth: date_of_birth.ok_or(Error::BadRequest("date_of_birth required"))?,
        invitation_code,
    })
}

pub struct ChairReg {
    pub name: String,
    pub model: String,
    pub chair_register_token: String,
}

pub fn parse_chair_reg(body: &[u8]) -> Result<ChairReg, Error> {
    let mut name = None;
    let mut model = None;
    let mut tok = None;
    let mut p = Parser::new(body);
    p.parse_object(|k, p| match k {
        "name" => {
            name = Some(p.parse_string()?);
            Ok(())
        }
        "model" => {
            model = Some(p.parse_string()?);
            Ok(())
        }
        "chair_register_token" => {
            tok = Some(p.parse_string()?);
            Ok(())
        }
        _ => p.skip_value(),
    })?;
    Ok(ChairReg {
        name: name.ok_or(Error::BadRequest("name required"))?,
        model: model.ok_or(Error::BadRequest("model required"))?,
        chair_register_token: tok.ok_or(Error::BadRequest("chair_register_token required"))?,
    })
}

pub struct Buf(pub String);

impl Buf {
    pub fn new() -> Self {
        Self(String::with_capacity(256))
    }
    pub fn push_str(&mut self, s: &str) {
        self.0.push_str(s);
    }
    pub fn obj_open(&mut self) {
        self.0.push('{');
    }
    pub fn obj_close(&mut self) {
        self.0.push('}');
    }
    pub fn arr_open(&mut self) {
        self.0.push('[');
    }
    pub fn arr_close(&mut self) {
        self.0.push(']');
    }
    pub fn comma(&mut self) {
        self.0.push(',');
    }
    pub fn key(&mut self, k: &str) {
        self.str(k);
        self.0.push(':');
    }
    pub fn str(&mut self, s: &str) {
        self.0.push('"');
        for c in s.chars() {
            match c {
                '"' => self.0.push_str("\\\""),
                '\\' => self.0.push_str("\\\\"),
                '\n' => self.0.push_str("\\n"),
                '\r' => self.0.push_str("\\r"),
                '\t' => self.0.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    use std::fmt::Write as _;
                    let _ = write!(self.0, "\\u{cp:04x}", cp = c as u32);
                }
                c => self.0.push(c),
            }
        }
        self.0.push('"');
    }
    pub fn i32(&mut self, n: i32) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{n}");
    }
    pub fn i64(&mut self, n: i64) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{n}");
    }
    pub fn f64(&mut self, n: f64) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{n}");
    }
    pub fn bool(&mut self, v: bool) {
        self.0.push_str(if v { "true" } else { "false" });
    }
    pub fn null(&mut self) {
        self.0.push_str("null");
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }
}

pub fn error_body(message: &str) -> Vec<u8> {
    let mut b = Buf::new();
    b.obj_open();
    b.key("message");
    b.str(message);
    b.obj_close();
    b.into_bytes()
}
