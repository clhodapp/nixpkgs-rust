//! JSON conversion for `builtins.toJSON`, `builtins.fromJSON` and structured
//! derivation attributes. Nix serialises with nlohmann::json, so the output
//! format (compact, keys sorted, shortest round-trip floats) follows it.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::attrs::{Attr, Attrs};
use crate::error::{Context as _, Error, R};
use crate::ops::coerce_to_string;
use crate::string::Context;
use crate::symbol::{intern, known, name};
use crate::value::{Pos, Value, call, force};

/// Writes a JSON string literal the way nlohmann::json escapes it.
pub fn write_json_string(out: &mut String, s: &[u8]) {
    let text = String::from_utf8_lossy(s);
    out.reserve(text.len() + 2);
    out.push('"');
    // Copy runs of characters that need no escape in one go; every
    // character that does is ASCII, so byte offsets are character
    // boundaries.
    let b = text.as_bytes();
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        if c >= 0x20 && c != b'"' && c != b'\\' {
            continue;
        }
        out.push_str(&text[start..i]);
        match c {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            c => write!(out, "\\u{:04x}", c).unwrap(),
        }
        start = i + 1;
    }
    out.push_str(&text[start..]);
    out.push('"');
}

/// Formats a float the way nlohmann::json does: the shortest digits that
/// round-trip, in fixed notation when the decimal point falls between
/// positions -4 and 15, with `.0` added to integral values, and in exponent
/// notation otherwise.
pub fn format_json_float(f: f64) -> String {
    if !f.is_finite() {
        return "null".to_string();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let e = format!("{:e}", f.abs());
    let (mant, exp) = e.split_once('e').unwrap();
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let k = digits.len() as i32;
    // The value is 0.d1d2...dk * 10^n.
    let n = exp.parse::<i32>().unwrap() + 1;
    let mut out = String::new();
    if f < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 15 {
        out.push_str(&digits);
        for _ in 0..(n - k) {
            out.push('0');
        }
        out.push_str(".0");
    } else if 0 < n && n <= 15 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -4 < n && n <= 0 {
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let x = n - 1;
        write!(out, "e{}{:02}", if x < 0 { '-' } else { '+' }, x.abs()).unwrap();
    }
    out
}

/// Serialises an object whose members are already JSON text.
pub fn object_to_string(members: &BTreeMap<String, String>) -> String {
    let mut out = String::from("{");
    for (n, (k, v)) in members.iter().enumerate() {
        if n > 0 {
            out.push(',');
        }
        write_json_string(&mut out, k.as_bytes());
        out.push(':');
        out.push_str(v);
    }
    out.push('}');
    out
}

/// Converts a value to JSON text, collecting the context of its strings.
/// Paths are copied into the store when `copy_to_store` is set.
pub fn value_to_json(v: &Value, copy_to_store: bool, ctx: &mut Context) -> R<String> {
    let mut out = String::new();
    to_json(v, copy_to_store, ctx, &mut out)?;
    Ok(out)
}

fn to_json(v: &Value, copy: bool, ctx: &mut Context, out: &mut String) -> R<()> {
    let v = force(v)?;
    let _d = crate::value::depth_guard()?;
    match &v {
        Value::Int(i) => write!(out, "{i}").unwrap(),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Null => out.push_str("null"),
        Value::Float(f) => out.push_str(&format_json_float(*f)),
        Value::Str(s) => {
            if let Err(e) = std::str::from_utf8(&s.s) {
                return Err(Error::eval(format!(
                    "JSON serialization error: [json.exception.type_error.316] invalid UTF-8 byte at index {}",
                    e.valid_up_to()
                )));
            }
            ctx.extend(s.ctx_iter().cloned());
            write_json_string(out, &s.s);
        }
        Value::Path(p) => {
            if copy {
                let sp = crate::store::copy_path_to_store(p, ctx)?;
                write_json_string(out, sp.as_bytes());
            } else {
                write_json_string(out, p.as_bytes());
            }
        }
        Value::Attrs(a) => {
            if let Some(f) = a.get(known::toString) {
                let r = call(f, v.clone())?;
                let s = coerce_to_string(&r, ctx, false, false, true)?;
                write_json_string(out, &s);
                return Ok(());
            }
            if let Some(o) = a.get(known::outPath) {
                return to_json(o, copy, ctx, out);
            }
            out.push('{');
            for (n, at) in a.by_name().into_iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                let k = name(at.k);
                write_json_string(out, k.as_bytes());
                out.push(':');
                to_json(&at.v, copy, ctx, out).context(|| format!("while evaluating attribute '{k}'"))?;
            }
            out.push('}');
        }
        Value::List(l) => {
            out.push('[');
            for (n, x) in l.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                to_json(x, copy, ctx, out).context(|| format!("while evaluating list element at index {n}"))?;
            }
            out.push(']');
        }
        other => {
            return Err(Error::eval(format!("cannot convert {} to JSON", other.show_type())));
        }
    }
    Ok(())
}

/// Parses JSON text into a value, as `builtins.fromJSON` does.
pub fn parse_json(s: &[u8]) -> R<Value> {
    let mut p = Parser { s, i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != s.len() {
        return Err(p.err("unexpected trailing characters"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &str) -> Box<Error> {
        Error::eval(format!("[json.exception.parse_error.101] parse error at byte {}: {msg}", self.i + 1))
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn expect(&mut self, lit: &[u8]) -> R<()> {
        if self.s[self.i..].starts_with(lit) {
            self.i += lit.len();
            Ok(())
        } else {
            Err(self.err("syntax error"))
        }
    }

    fn value(&mut self) -> R<Value> {
        match self.peek() {
            Some(b'{') => {
                self.i += 1;
                let mut entries: Vec<Attr> = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(Value::attrs(Attrs::new()));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    self.expect(b":")?;
                    self.ws();
                    let v = self.value()?;
                    entries.push(Attr {
                        k: intern(&String::from_utf8_lossy(&k)),
                        v,
                        pos: Pos::default(),
                    });
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            break;
                        }
                        _ => return Err(self.err("syntax error while parsing object")),
                    }
                }
                Ok(Value::attrs(Attrs::from_vec_last_wins(entries)))
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(Value::list(items));
                }
                loop {
                    self.ws();
                    items.push(self.value()?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            break;
                        }
                        _ => return Err(self.err("syntax error while parsing array")),
                    }
                }
                Ok(Value::list(items))
            }
            Some(b'"') => Ok(Value::str(self.string()?)),
            Some(b't') => self.expect(b"true").map(|_| Value::Bool(true)),
            Some(b'f') => self.expect(b"false").map(|_| Value::Bool(false)),
            Some(b'n') => self.expect(b"null").map(|_| Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err("syntax error while parsing value")),
        }
    }

    fn number(&mut self) -> R<Value> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => {}
                b'.' | b'e' | b'E' | b'+' | b'-' => is_float = true,
                _ => break,
            }
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap();
        if !is_float {
            return match text.parse::<i64>() {
                Ok(i) => Ok(Value::Int(i)),
                Err(_) if text.starts_with('-') => Err(Error::eval(format!(
                    "[json.exception.out_of_range.406] number overflow parsing '{text}'"
                ))),
                Err(_) => Err(Error::eval(format!(
                    "unsigned json number {text} outside of Nix integer range"
                ))),
            };
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::Float(f)),
            _ => Err(Error::eval(format!(
                "[json.exception.out_of_range.406] number overflow parsing '{text}'"
            ))),
        }
    }

    fn hex4(&mut self) -> R<u32> {
        let h = self
            .s
            .get(self.i..self.i + 4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or_else(|| self.err("invalid string: '\\u' must be followed by 4 hex digits"))?;
        self.i += 4;
        Ok(h)
    }

    fn string(&mut self) -> R<Vec<u8>> {
        self.expect(b"\"")?;
        let mut out = Vec::new();
        loop {
            let c = self.peek().ok_or_else(|| self.err("invalid string: missing closing quote"))?;
            self.i += 1;
            match c {
                b'"' => {
                if out.contains(&0) {
                    return Err(Error::eval("input string contains NUL byte"));
                }
                return Ok(out);
            }
                b'\\' => {
                    let e = self.peek().ok_or_else(|| self.err("invalid string"))?;
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xd800..0xdc00).contains(&cp) {
                                self.expect(b"\\u")?;
                                let lo = self.hex4()?;
                                cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00);
                            }
                            let ch = char::from_u32(cp).ok_or_else(|| self.err("invalid string"))?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(self.err("invalid string: forbidden character after backslash")),
                    }
                }
                c if c < 0x20 => return Err(self.err("invalid string: control character must be escaped")),
                c => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_match_nix_to_json() {
        // Observed from `builtins.toJSON` in Nix 2.34.9.
        let cases = [
            (0.1, "0.1"),
            (1.0, "1.0"),
            (1.5, "1.5"),
            (1.0e20, "1e+20"),
            (1.0e15, "1e+15"),
            (1.0e16, "1e+16"),
            (123456789.123, "123456789.123"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (3.0e-7, "3e-07"),
        ];
        for (f, s) in cases {
            assert_eq!(format_json_float(f), s, "{f}");
        }
    }
}
