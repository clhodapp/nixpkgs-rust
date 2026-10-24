//! Printing values.
//!
//! [`print_strict`] produces the output of `nix-instantiate --eval --strict`
//! (`printAmbiguous` in the evaluator), which the Nix language tests compare
//! against. [`show_value_short`] is the bounded printer used inside error
//! messages; it never forces anything.

use std::collections::HashSet;
use std::fmt::Write;

use crate::error::R;
use crate::symbol::name;
use crate::value::{Value, force};

/// Formats a float like a C++ stream with default settings: `%g` in C, with
/// six significant digits.
pub fn format_float(f: f64) -> String {
    format_g(f, 6)
}

/// The `%.<precision>g` conversion of C.
pub fn format_g(f: f64, precision: usize) -> String {
    if f.is_nan() {
        return if f.is_sign_negative() { "-nan".into() } else { "nan".into() };
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf".into() } else { "inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let p = precision.max(1);
    let e = format!("{:.*e}", p - 1, f);
    let (mant, exp) = e.split_once('e').unwrap();
    let x: i32 = exp.parse().unwrap();
    if x < -4 || x >= p as i32 {
        let mant = strip_zeros(mant);
        let sign = if x < 0 { '-' } else { '+' };
        format!("{mant}e{sign}{:02}", x.abs())
    } else {
        let decimals = (p as i32 - 1 - x).max(0) as usize;
        strip_zeros(&format!("{f:.decimals$}")).to_string()
    }
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// Writes a string literal with the escaping Nix prints.
pub fn write_string(out: &mut String, s: &[u8]) {
    out.push('"');
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'$' if s.get(i + 1) == Some(&b'{') => out.push_str("\\$"),
            b'$' => out.push('$'),
            _ => {
                let start = i;
                while i < s.len() && !matches!(s[i], b'"' | b'\\' | b'\n' | b'\r' | b'\t' | b'$') {
                    i += 1;
                }
                out.push_str(&String::from_utf8_lossy(&s[start..i]));
                continue;
            }
        }
        i += 1;
    }
    out.push('"');
}

fn is_reserved(s: &str) -> bool {
    matches!(s, "if" | "then" | "else" | "assert" | "with" | "let" | "in" | "rec" | "inherit")
}

/// Writes an attribute name, quoting it when it is not an identifier.
pub fn write_identifier(out: &mut String, s: &str) {
    let b = s.as_bytes();
    let plain = !b.is_empty()
        && !is_reserved(s)
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'\'' || c == b'-');
    if plain {
        out.push_str(s);
    } else {
        write_string(out, s.as_bytes());
    }
}

/// The output of `nix-instantiate --eval --strict`. Forces everything it prints.
///
/// Like Nix, it prints a set it has already printed as `«repeated»`, and a
/// list held by a value it has already printed likewise. A set is
/// recognised by its contents; a list by the value that holds it: the thunk
/// it was reached through, or the list itself when it was stored directly.
/// The top-level value is a value of its own, as the evaluator's result is.
pub fn print_strict(v: &Value) -> R<String> {
    let mut out = String::new();
    let mut seen = HashSet::new();
    print_rec(&mut out, v, &mut seen, true)?;
    Ok(out)
}

fn print_rec(out: &mut String, held: &Value, seen: &mut HashSet<usize>, top: bool) -> R<()> {
    let v = force(held)?;
    let _d = crate::value::depth_guard()?;
    if let Value::List(l) = &v {
        if !l.is_empty() && !top {
            let key = match held {
                Value::Thunk(t) => std::rc::Rc::as_ptr(t) as *const u8 as usize,
                _ => std::rc::Rc::as_ptr(l) as *const u8 as usize,
            };
            if !seen.insert(key) {
                out.push_str("«repeated»");
                return Ok(());
            }
        }
        out.push_str("[ ");
        for x in l.iter() {
            print_rec(out, x, seen, false)?;
            out.push(' ');
        }
        out.push(']');
        return Ok(());
    }
    match &v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => write!(out, "{i}").unwrap(),
        Value::Float(f) => out.push_str(&format_float(*f)),
        Value::Str(s) => write_string(out, &s.s),
        Value::Path(p) => out.push_str(p),
        Value::Attrs(a) => {
            if !a.is_empty() && !seen.insert(std::rc::Rc::as_ptr(a) as *const u8 as usize) {
                out.push_str("«repeated»");
                return Ok(());
            }
            out.push_str("{ ");
            for at in a.by_name() {
                write_identifier(out, &name(at.k));
                out.push_str(" = ");
                print_rec(out, &at.v, seen, false)?;
                out.push_str("; ");
            }
            out.push('}');
        }
        Value::List(_) => unreachable!(),
        Value::Lambda(_) => out.push_str("<LAMBDA>"),
        Value::Prim(p) if p.args.is_empty() => out.push_str("<PRIMOP>"),
        Value::Prim(_) => out.push_str("<PRIMOP-APP>"),
        Value::Thunk(_) => out.push_str("<CODE>"),
    }
    Ok(())
}

/// A short rendering for error messages: never forces, bounded in depth and size.
pub fn show_value_short(v: &Value) -> String {
    let mut out = String::new();
    show_rec(&mut out, v, 0);
    out
}

fn show_rec(out: &mut String, v: &Value, depth: usize) {
    if out.len() > 400 {
        return;
    }
    match v {
        Value::Thunk(t) => match &*t.0.borrow() {
            crate::value::ThunkState::Done(x) => show_rec(out, x, depth),
            _ => out.push_str("«thunk»"),
        },
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => write!(out, "{i}").unwrap(),
        Value::Float(f) => out.push_str(&format_float(*f)),
        Value::Str(s) => {
            let b = &s.s;
            if b.len() > 64 {
                write_string(out, &b[..64]);
                write!(out, " «{} bytes elided»", b.len() - 64).unwrap();
            } else {
                write_string(out, b);
            }
        }
        Value::Path(p) => out.push_str(p),
        Value::Attrs(a) => {
            if depth >= 2 {
                out.push_str("{ ... }");
                return;
            }
            if a.is_empty() {
                out.push_str("{ }");
                return;
            }
            out.push_str("{ ");
            for (n, at) in a.by_name().into_iter().enumerate() {
                if n >= 8 {
                    write!(out, "«{} attributes elided» ", a.len() - n).unwrap();
                    break;
                }
                write_identifier(out, &name(at.k));
                out.push_str(" = ");
                show_rec(out, &at.v, depth + 1);
                out.push_str("; ");
            }
            out.push('}');
        }
        Value::List(l) => {
            if depth >= 2 {
                out.push_str("[ ... ]");
                return;
            }
            if l.is_empty() {
                out.push_str("[ ]");
                return;
            }
            out.push_str("[ ");
            for (n, x) in l.iter().enumerate() {
                if n >= 8 {
                    write!(out, "«{} items elided» ", l.len() - n).unwrap();
                    break;
                }
                show_rec(out, x, depth + 1);
                out.push(' ');
            }
            out.push(']');
        }
        Value::Lambda(_) => out.push_str("«lambda»"),
        Value::Prim(p) if p.args.is_empty() => write!(out, "«primop {}»", p.op.name).unwrap(),
        Value::Prim(p) => write!(out, "«partially applied primop {}»", p.op.name).unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_like_cpp_streams() {
        assert_eq!(format_float(1.5), "1.5");
        assert_eq!(format_float(0.1), "0.1");
        assert_eq!(format_float(1.0e20), "1e+20");
        assert_eq!(format_float(123456789.0), "1.23457e+08");
        assert_eq!(format_float(1.0), "1");
        assert_eq!(format_float(0.0001), "0.0001");
        assert_eq!(format_float(0.00001), "1e-05");
        assert_eq!(format_float(100000.0), "100000");
        assert_eq!(format_float(1000000.0), "1e+06");
    }

    #[test]
    fn strings_escape_interpolation_only() {
        let mut s = String::new();
        write_string(&mut s, b"a$b${c}\"\n");
        assert_eq!(s, r#""a$b\${c}\"\n""#);
    }
}
