//! `builtins.toXML`, in the format Nix writes it (without locations).

use std::collections::HashSet;
use std::fmt::Write;

use crate::error::R;
use crate::ops::is_derivation;
use crate::string::Context;
use crate::symbol::{known, name};
use crate::value::{Value, force};

fn escape(s: &[u8]) -> String {
    let text = String::from_utf8_lossy(s);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
    out
}

struct W {
    out: String,
    depth: usize,
    drvs_seen: HashSet<Vec<u8>>,
}

impl W {
    fn indent(&mut self) {
        for _ in 0..self.depth {
            self.out.push_str("  ");
        }
    }

    fn empty(&mut self, tag: &str, attrs: &[(&str, String)]) {
        self.indent();
        write!(self.out, "<{tag}").unwrap();
        for (k, v) in attrs {
            write!(self.out, " {k}=\"{v}\"").unwrap();
        }
        self.out.push_str(" />\n");
    }

    fn open(&mut self, tag: &str, attrs: &[(&str, String)]) {
        self.indent();
        write!(self.out, "<{tag}").unwrap();
        for (k, v) in attrs {
            write!(self.out, " {k}=\"{v}\"").unwrap();
        }
        self.out.push_str(">\n");
        self.depth += 1;
    }

    fn close(&mut self, tag: &str) {
        self.depth -= 1;
        self.indent();
        writeln!(self.out, "</{tag}>").unwrap();
    }
}

fn attrs_body(w: &mut W, a: &crate::attrs::Attrs, ctx: &mut Context) -> R<()> {
    for at in a.by_name() {
        w.open("attr", &[("name", escape(name(at.k).as_bytes()))]);
        value(w, &at.v, ctx)?;
        w.close("attr");
    }
    Ok(())
}

fn value(w: &mut W, v: &Value, ctx: &mut Context) -> R<()> {
    let v = force(v)?;
    match &v {
        Value::Int(i) => w.empty("int", &[("value", i.to_string())]),
        Value::Bool(b) => w.empty("bool", &[("value", (if *b { "true" } else { "false" }).to_string())]),
        Value::Str(s) => {
            ctx.extend(s.ctx_iter().cloned());
            w.empty("string", &[("value", escape(&s.s))]);
        }
        Value::Path(p) => w.empty("path", &[("value", escape(p.as_bytes()))]),
        Value::Null => w.empty("null", &[]),
        Value::Float(f) => w.empty("float", &[("value", crate::print::format_float(*f))]),
        Value::Attrs(a) => {
            if is_derivation(a)? {
                let mut xa = Vec::new();
                let mut drv_path = Vec::new();
                if let Some(d) = a.get(known::drvPath)
                    && let Value::Str(s) = force(d)?
                {
                    drv_path = s.s.to_vec();
                    xa.push(("drvPath", escape(&s.s)));
                }
                if let Some(o) = a.get(known::outPath)
                    && let Value::Str(s) = force(o)?
                {
                    xa.push(("outPath", escape(&s.s)));
                }
                w.open("derivation", &xa);
                if !drv_path.is_empty() && w.drvs_seen.insert(drv_path) {
                    attrs_body(w, a, ctx)?;
                } else {
                    w.empty("repeated", &[]);
                }
                w.close("derivation");
            } else {
                w.open("attrs", &[]);
                attrs_body(w, a, ctx)?;
                w.close("attrs");
            }
        }
        Value::List(l) => {
            w.open("list", &[]);
            for x in l.iter() {
                value(w, x, ctx)?;
            }
            w.close("list");
        }
        Value::Lambda(l) => {
            w.open("function", &[]);
            match &l.info.formals {
                Some(f) => {
                    let mut xa = Vec::new();
                    if let Some(a) = l.info.arg {
                        xa.push(("name", escape(name(a).as_bytes())));
                    }
                    if f.ellipsis {
                        xa.push(("ellipsis", "1".to_string()));
                    }
                    w.open("attrspat", &xa);
                    let mut names: Vec<String> = f.names.iter().map(|(k, _, _)| name(*k).to_string()).collect();
                    names.sort();
                    for n in names {
                        w.empty("attr", &[("name", escape(n.as_bytes()))]);
                    }
                    w.close("attrspat");
                }
                None => {
                    let a = l.info.arg.map(|a| name(a).to_string()).unwrap_or_default();
                    w.empty("varpat", &[("name", escape(a.as_bytes()))]);
                }
            }
            w.close("function");
        }
        Value::Prim(_) => {}
        Value::Thunk(_) => w.empty("unevaluated", &[]),
    }
    Ok(())
}

/// Renders a value as Nix's XML, collecting the context of its strings.
pub fn value_to_xml(v: &Value, ctx: &mut Context, _strict: bool) -> R<String> {
    let mut w = W {
        out: String::from("<?xml version='1.0' encoding='utf-8'?>\n"),
        depth: 0,
        drvs_seen: HashSet::new(),
    };
    w.open("expr", &[]);
    value(&mut w, v, ctx)?;
    w.close("expr");
    Ok(w.out)
}
