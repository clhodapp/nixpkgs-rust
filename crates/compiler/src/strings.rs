//! The literal text of strings, with the escaping and indentation rules of
//! the Nix lexer and parser.
//!
//! rnix keeps string content as raw source text. Nix's lexer splits an
//! indented string into runs of text, which count towards the indentation
//! that `stripIndentation` removes, and escape sequences, which do not.
//! This module repeats that split on the raw text and then strips the
//! indentation the way the parser does.

use rnix::ast::{self, AstToken, InterpolPart};

/// A piece of a string: literal bytes or an interpolated expression.
pub enum Part {
    Lit(Vec<u8>),
    Interp(ast::Interpol),
}

/// Unescapes the content of a double-quoted string like Nix's `unescapeStr`:
/// `\n`, `\r` and `\t` are control characters, a backslash before anything
/// else is dropped, and CR and CR LF become LF.
pub fn unescape_str(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c == b'\\' {
            if i < s.len() {
                let e = s[i];
                i += 1;
                out.push(match e {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    e => e,
                });
            }
        } else if c == b'\r' {
            out.push(b'\n');
            if i < s.len() && s[i] == b'\n' {
                i += 1;
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A token of an indented string: text that counts towards indentation, an
/// escape that does not, or an interpolation.
enum Ind {
    Text(Vec<u8>),
    Escape(Vec<u8>),
    Interp(ast::Interpol),
}

/// The length of the longest run of plain indented-string text at `i`: the
/// lexer rule `([^$']|$[^{']|'[^'$])+`.
fn plain_run(s: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < s.len() {
        match s[j] {
            b'$' if j + 1 < s.len() && s[j + 1] != b'{' && s[j + 1] != b'\'' => j += 2,
            b'\'' if j + 1 < s.len() && s[j + 1] != b'\'' && s[j + 1] != b'$' => j += 2,
            b'$' | b'\'' => break,
            _ => j += 1,
        }
    }
    j - i
}

/// Splits raw indented-string content into tokens as the Nix lexer does,
/// taking the longest match at each position.
fn lex_indented(s: &[u8], out: &mut Vec<Ind>) {
    let mut i = 0;
    while i < s.len() {
        let run = plain_run(s, i);
        let (len, tok) = if s[i..].starts_with(b"''$") {
            (3, Ind::Escape(b"$".to_vec()))
        } else if s[i..].starts_with(b"'''") {
            (3, Ind::Escape(b"''".to_vec()))
        } else if s[i..].starts_with(b"''\\") && i + 3 < s.len() {
            (4, Ind::Escape(unescape_str(&s[i + 2..i + 4])))
        } else if s[i] == b'$' {
            (1, Ind::Escape(b"$".to_vec()))
        } else if s[i] == b'\'' {
            (1, Ind::Escape(b"'".to_vec()))
        } else {
            (0, Ind::Escape(Vec::new()))
        };
        if run > 0 && run >= len {
            out.push(Ind::Text(s[i..i + run].to_vec()));
            i += run;
        } else {
            out.push(tok);
            i += len.max(1);
        }
    }
}

/// `stripIndentation` from the Nix parser.
fn strip_indentation(items: Vec<Ind>) -> Vec<Part> {
    let mut at_start = true;
    let mut min_indent = usize::MAX;
    let mut cur = 0usize;
    for it in &items {
        match it {
            Ind::Text(t) => {
                for &c in t {
                    if at_start {
                        if c == b' ' {
                            cur += 1;
                        } else if c == b'\n' {
                            cur = 0;
                        } else {
                            at_start = false;
                            min_indent = min_indent.min(cur);
                        }
                    } else if c == b'\n' {
                        at_start = true;
                        cur = 0;
                    }
                }
            }
            _ => {
                if at_start {
                    at_start = false;
                    min_indent = min_indent.min(cur);
                }
            }
        }
    }
    let mut out: Vec<Part> = Vec::new();
    let push_lit = |out: &mut Vec<Part>, s: Vec<u8>| {
        if s.is_empty() {
            return;
        }
        if let Some(Part::Lit(prev)) = out.last_mut() {
            prev.extend_from_slice(&s);
        } else {
            out.push(Part::Lit(s));
        }
    };
    at_start = true;
    let mut dropped = 0usize;
    let n = items.len();
    for (idx, it) in items.into_iter().enumerate() {
        let last = idx + 1 == n;
        match it {
            Ind::Interp(e) => {
                at_start = false;
                dropped = 0;
                out.push(Part::Interp(e));
            }
            Ind::Escape(t) => {
                // An escape is a string token without indentation: it ends
                // the leading whitespace and is kept as it is.
                at_start = false;
                dropped = 0;
                let mut s2 = t;
                if last {
                    trim_last_line(&mut s2);
                }
                push_lit(&mut out, s2);
            }
            Ind::Text(t) => {
                let mut s2 = Vec::with_capacity(t.len());
                for c in t {
                    if at_start {
                        if c == b' ' {
                            if dropped >= min_indent {
                                s2.push(c);
                            }
                            dropped += 1;
                        } else if c == b'\n' {
                            dropped = 0;
                            s2.push(c);
                        } else {
                            at_start = false;
                            dropped = 0;
                            s2.push(c);
                        }
                    } else {
                        s2.push(c);
                        if c == b'\n' {
                            at_start = true;
                        }
                    }
                }
                if last {
                    trim_last_line(&mut s2);
                }
                push_lit(&mut out, s2);
            }
        }
    }
    out
}

/// Removes the last line when it consists only of spaces.
fn trim_last_line(s: &mut Vec<u8>) {
    if let Some(p) = s.iter().rposition(|&c| c == b'\n')
        && s[p + 1..].iter().all(|&c| c == b' ')
    {
        s.truncate(p + 1);
    }
}

/// The parts of a string literal after unescaping and, for indented strings,
/// indentation stripping. Adjacent literal parts are merged and empty ones
/// dropped.
pub fn string_parts(s: &ast::Str) -> Vec<Part> {
    use rowan::ast::AstNode;
    let multiline = s
        .syntax()
        .first_token()
        .is_some_and(|t| t.text() == "''");
    if !multiline {
        let mut out: Vec<Part> = Vec::new();
        for p in s.parts() {
            match p {
                InterpolPart::Literal(t) => {
                    let u = restore_raw(unescape_str(t.syntax().text().as_bytes()));
                    if let Some(Part::Lit(prev)) = out.last_mut() {
                        prev.extend_from_slice(&u);
                    } else if !u.is_empty() {
                        out.push(Part::Lit(u));
                    }
                }
                InterpolPart::Interpolation(i) => out.push(Part::Interp(i)),
            }
        }
        return out;
    }
    let mut items = Vec::new();
    let mut first = true;
    for p in s.parts() {
        match p {
            InterpolPart::Literal(t) => {
                let mut raw = t.syntax().text().as_bytes();
                if first {
                    // The opening `''` takes spaces and one newline with it.
                    let spaces = raw.iter().take_while(|&&c| c == b' ').count();
                    if raw.get(spaces) == Some(&b'\n') {
                        raw = &raw[spaces + 1..];
                    }
                }
                lex_indented(raw, &mut items);
            }
            InterpolPart::Interpolation(i) => items.push(Ind::Interp(i)),
        }
        first = false;
    }
    strip_indentation(items)
        .into_iter()
        .map(|p| match p {
            Part::Lit(b) => Part::Lit(restore_raw(b)),
            other => other,
        })
        .collect()
}

/// The first code point used to carry a byte that is not valid UTF-8
/// through the parser, which only accepts text. Byte `b` becomes code point
/// `RAW_BASE + b`, in a private-use plane that Nix sources do not use.
const RAW_BASE: u32 = 0x10FF00;

/// Decodes source text without losing invalid UTF-8: each byte that is not
/// part of a valid sequence becomes a private-use code point that
/// [`restore_raw`] turns back into the byte.
pub fn decode_lossless(mut b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    loop {
        match std::str::from_utf8(b) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                let (good, rest) = b.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(good).unwrap());
                let bad = e.error_len().unwrap_or(rest.len());
                for &x in &rest[..bad] {
                    out.push(char::from_u32(RAW_BASE + x as u32).unwrap());
                }
                b = &rest[bad..];
            }
        }
    }
}

/// Turns the code points [`decode_lossless`] introduced back into bytes.
pub fn restore_raw(b: Vec<u8>) -> Vec<u8> {
    // RAW_BASE..RAW_BASE+0xff encode as F4 8F BC..BF xx.
    if !b.windows(2).any(|w| w == [0xf4, 0x8f]) {
        return b;
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if i + 3 < b.len() && b[i] == 0xf4 && b[i + 1] == 0x8f && (0xbc..=0xbf).contains(&b[i + 2]) {
            let cp = 0x10F000 | ((b[i + 2] as u32 & 0x3f) << 6) | (b[i + 3] as u32 & 0x3f);
            if cp >= RAW_BASE {
                out.push((cp - RAW_BASE) as u8);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Writes bytes as a Rust byte string literal.
pub fn rust_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() + 3);
    s.push_str("b\"");
    for &c in b {
        match c {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            b'\t' => s.push_str("\\t"),
            0x20..=0x7e => s.push(c as char),
            _ => s.push_str(&format!("\\x{c:02x}")),
        }
    }
    s.push('"');
    s
}

/// Writes a string as a Rust string literal.
pub fn rust_str(t: &str) -> String {
    format!("{t:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowan::ast::AstNode;

    fn lit(src: &str) -> Vec<u8> {
        let root = rnix::Root::parse(src).tree();
        let e = root.expr().unwrap();
        let s = ast::Str::cast(e.syntax().clone()).unwrap();
        let parts = string_parts(&s);
        match parts.as_slice() {
            [] => Vec::new(),
            [Part::Lit(b)] => b.clone(),
            _ => panic!("not a literal"),
        }
    }

    #[test]
    fn plain_escapes() {
        assert_eq!(lit(r#""a\nb\$c$${x}""#), b"a\nb$c$${x}");
        assert_eq!(lit("\"a\r\nb\""), b"a\nb");
    }

    #[test]
    fn indented() {
        assert_eq!(lit("''\n  a\n    b\n  ''"), b"a\n  b\n");
        assert_eq!(lit("''\n  ''$x '''\n  ''"), b"$x ''\n");
        assert_eq!(lit("''  a''"), b"a");
        assert_eq!(lit("''\n  a\n  ''\\tb\n''"), b"a\n\tb\n");
    }
}
