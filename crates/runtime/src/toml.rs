//! `builtins.fromTOML`.

use crate::attrs::{Attr, Attrs};
use crate::error::{Error, R};
use crate::symbol::intern;
use crate::value::{Pos, Value};

fn no_nul(s: &str) -> R<()> {
    if s.contains('\0') {
        return Err(Error::eval("while parsing TOML: input string contains NUL byte"));
    }
    Ok(())
}

fn convert(v: toml::Value) -> R<Value> {
    Ok(match v {
        toml::Value::String(s) => {
            no_nul(&s)?;
            Value::str(s.into_bytes())
        }
        toml::Value::Integer(i) => Value::Int(i),
        toml::Value::Float(f) => Value::Float(f),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(_) => {
            return Err(Error::eval(
                "while parsing TOML: Dates and times are not supported",
            ));
        }
        toml::Value::Array(a) => Value::list(a.into_iter().map(convert).collect::<R<_>>()?),
        toml::Value::Table(t) => table(t)?,
    })
}

fn table(t: toml::Table) -> R<Value> {
    let mut e = Vec::with_capacity(t.len());
    for (k, v) in t {
        no_nul(&k)?;
        e.push(Attr {
            k: intern(&k),
            v: convert(v)?,
            pos: Pos::default(),
        });
    }
    Ok(Value::attrs(Attrs::from_vec_first_wins(e)))
}

/// Parses TOML text into a set.
pub fn parse_toml(s: &str) -> R<Value> {
    let t: toml::Table = s
        .parse()
        .map_err(|e: toml::de::Error| Error::eval(format!("while parsing TOML: {}", e.message())))?;
    table(t)
}
