//! Reading store derivations (`.drv` files) in the ATerm format, for
//! derivations that exist in the store rather than in this evaluation.

use std::collections::{BTreeMap, BTreeSet};

use crate::derivation::{Derivation, Output};
use crate::error::{Error, R};
use crate::store::{Algo, Hash, Method};

struct P<'a> {
    s: &'a [u8],
    i: usize,
    path: &'a str,
}

impl P<'_> {
    fn err(&self) -> Box<Error> {
        Error::eval(format!("cannot parse derivation '{}' at byte {}", self.path, self.i))
    }

    fn lit(&mut self, l: &[u8]) -> R<()> {
        if self.s[self.i..].starts_with(l) {
            self.i += l.len();
            Ok(())
        } else {
            Err(self.err())
        }
    }

    fn try_lit(&mut self, l: &[u8]) -> bool {
        if self.s[self.i..].starts_with(l) {
            self.i += l.len();
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> R<Vec<u8>> {
        self.lit(b"\"")?;
        let mut out = Vec::new();
        loop {
            let c = *self.s.get(self.i).ok_or_else(|| self.err())?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = *self.s.get(self.i).ok_or_else(|| self.err())?;
                    self.i += 1;
                    out.push(match e {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        e => e,
                    });
                }
                c => out.push(c),
            }
        }
    }

    fn str_utf8(&mut self) -> R<String> {
        Ok(String::from_utf8_lossy(&self.string()?).into_owned())
    }

    /// Parses `[item,item,...]`.
    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> R<T>) -> R<Vec<T>> {
        self.lit(b"[")?;
        let mut v = Vec::new();
        if self.try_lit(b"]") {
            return Ok(v);
        }
        loop {
            v.push(item(self)?);
            if self.try_lit(b"]") {
                return Ok(v);
            }
            self.lit(b",")?;
        }
    }
}

fn parse_method_algo(s: &str) -> R<(Method, Algo)> {
    let (method, rest) = if let Some(r) = s.strip_prefix("r:") {
        (Method::Recursive, r)
    } else if let Some(r) = s.strip_prefix("text:") {
        (Method::Text, r)
    } else {
        (Method::Flat, s)
    };
    let algo = Algo::parse(rest).ok_or_else(|| Error::eval(format!("unknown hash algorithm '{rest}'")))?;
    Ok((method, algo))
}

/// Parses the text of a `.drv` file.
pub fn parse(s: &[u8], path: &str) -> R<Derivation> {
    let mut p = P { s, i: 0, path };
    p.lit(b"Derive(")?;
    let name = path
        .rsplit('/')
        .next()
        .and_then(|b| b.get(33..))
        .and_then(|n| n.strip_suffix(".drv"))
        .unwrap_or("")
        .to_string();
    let mut drv = Derivation {
        name,
        ..Default::default()
    };
    let outs = p.list(|p| {
        p.lit(b"(")?;
        let n = p.str_utf8()?;
        p.lit(b",")?;
        let path = p.str_utf8()?;
        p.lit(b",")?;
        let algo = p.str_utf8()?;
        p.lit(b",")?;
        let hash = p.str_utf8()?;
        p.lit(b")")?;
        let out = if algo.is_empty() {
            if path.is_empty() { Output::Deferred } else { Output::InputAddressed(path) }
        } else if hash == "impure" {
            let (method, algo) = parse_method_algo(&algo)?;
            Output::Impure { method, algo }
        } else if hash.is_empty() {
            let (method, algo) = parse_method_algo(&algo)?;
            Output::Floating { method, algo }
        } else {
            let (method, algo) = parse_method_algo(&algo)?;
            Output::Fixed {
                method,
                hash: Hash::parse(&hash, Some(algo))?,
            }
        };
        Ok((n, out))
    })?;
    drv.outputs = outs.into_iter().collect();
    p.lit(b",")?;
    let inputs = p.list(|p| {
        p.lit(b"(")?;
        let d = p.str_utf8()?;
        p.lit(b",")?;
        let outs: BTreeSet<String> = p.list(|p| p.str_utf8())?.into_iter().collect();
        p.lit(b")")?;
        Ok((d, outs))
    })?;
    drv.input_drvs = inputs.into_iter().collect::<BTreeMap<_, _>>();
    p.lit(b",")?;
    drv.input_srcs = p.list(|p| p.str_utf8())?.into_iter().collect();
    p.lit(b",")?;
    drv.platform = p.str_utf8()?;
    p.lit(b",")?;
    drv.builder = p.string()?;
    p.lit(b",")?;
    drv.args = p.list(|p| p.string())?;
    p.lit(b",")?;
    let env = p.list(|p| {
        p.lit(b"(")?;
        let k = p.str_utf8()?;
        p.lit(b",")?;
        let v = p.string()?;
        p.lit(b")")?;
        Ok((k, v))
    })?;
    for (k, v) in env {
        if k == "__json" {
            drv.structured = Some(String::from_utf8_lossy(&v).into_owned());
        } else {
            drv.env.insert(k, v);
        }
    }
    p.lit(b")")?;
    Ok(drv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let text = br#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x","","")],[("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-y.drv",["dev","out"])],["/nix/store/cccccccccccccccccccccccccccccccc-s"],"x86_64-linux","/bin/sh",["-c","echo \"hi\"\n"],[("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x")])"#;
        let d = parse(text, "/nix/store/dddddddddddddddddddddddddddddddd-x.drv").unwrap();
        assert_eq!(d.name, "x");
        assert_eq!(d.unparse(false, None), text.to_vec());
    }
}
