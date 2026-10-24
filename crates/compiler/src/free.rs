//! Free variables of expressions.
//!
//! The code generator uses these to decide which bindings of a `let` or a
//! recursive set refer to other bindings of the same group and so need a
//! thunk that is allocated first and filled in afterwards.

use std::collections::HashSet;

use rnix::ast::{self, HasEntry};

/// The names an expression refers to that it does not bind itself.
pub fn free_vars(e: &ast::Expr) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut bound = Vec::new();
    walk(e, &mut bound, &mut out);
    out
}

/// The text of an identifier, including the keyword-like `or`.
pub fn ident_text(i: &ast::Ident) -> String {
    use rowan::ast::AstNode;
    i.syntax().text().to_string()
}

fn is_bound(bound: &[HashSet<String>], n: &str) -> bool {
    bound.iter().any(|s| s.contains(n))
}

fn refer(bound: &[HashSet<String>], out: &mut HashSet<String>, n: &str) {
    if !is_bound(bound, n) {
        out.insert(n.to_string());
    }
}

/// The names bound by the static attributes of a set or `let`: the first
/// component of each attribute path and the names of `inherit` entries.
pub fn bound_names(h: &impl HasEntry) -> HashSet<String> {
    let mut names = HashSet::new();
    for e in h.entries() {
        match e {
            ast::Entry::AttrpathValue(av) => {
                if let Some(first) = av.attrpath().and_then(|p| p.attrs().next())
                    && let Some(n) = static_attr_name(&first)
                {
                    names.insert(n);
                }
            }
            ast::Entry::Inherit(inh) => {
                for a in inh.attrs() {
                    if let Some(n) = static_attr_name(&a) {
                        names.insert(n);
                    }
                }
            }
        }
    }
    names
}

/// The name of an attribute when it is known statically: an identifier, a
/// string without interpolation, or `${...}` around such a string.
pub fn static_attr_name(a: &ast::Attr) -> Option<String> {
    match a {
        ast::Attr::Ident(i) => Some(ident_text(i)),
        ast::Attr::Str(s) => static_string(s),
        ast::Attr::Dynamic(d) => match d.expr()? {
            ast::Expr::Str(s) => static_string(&s),
            _ => None,
        },
    }
}

fn static_string(s: &ast::Str) -> Option<String> {
    let parts = crate::strings::string_parts(s);
    match parts.as_slice() {
        [] => Some(String::new()),
        [crate::strings::Part::Lit(b)] => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

fn walk_entries(h: &impl HasEntry, bound: &mut Vec<HashSet<String>>, out: &mut HashSet<String>, inner: bool) {
    // `inner` is set for `let` and recursive sets: the values see the bindings.
    // `inherit x;` always refers outside; `inherit (e) x;` evaluates `e` inside.
    for e in h.entries() {
        match e {
            ast::Entry::AttrpathValue(av) => {
                if let Some(p) = av.attrpath() {
                    for a in p.attrs() {
                        walk_attr(&a, bound, out);
                    }
                }
                if let Some(v) = av.value() {
                    walk(&v, bound, out);
                }
            }
            ast::Entry::Inherit(inh) => match inh.from().and_then(|f| f.expr()) {
                Some(f) => walk(&f, bound, out),
                None => {
                    for a in inh.attrs() {
                        if let Some(n) = static_attr_name(&a) {
                            if inner {
                                // Refers to the enclosing scope, past this group.
                                let top = bound.pop();
                                refer(bound, out, &n);
                                if let Some(t) = top {
                                    bound.push(t);
                                }
                            } else {
                                refer(bound, out, &n);
                            }
                        }
                    }
                }
            },
        }
    }
}

fn walk_attr(a: &ast::Attr, bound: &mut Vec<HashSet<String>>, out: &mut HashSet<String>) {
    match a {
        ast::Attr::Ident(_) => {}
        ast::Attr::Str(s) => walk_str(s, bound, out),
        ast::Attr::Dynamic(d) => {
            if let Some(e) = d.expr() {
                walk(&e, bound, out);
            }
        }
    }
}

fn walk_str(s: &ast::Str, bound: &mut Vec<HashSet<String>>, out: &mut HashSet<String>) {
    for p in s.parts() {
        if let ast::InterpolPart::Interpolation(i) = p
            && let Some(e) = i.expr()
        {
            walk(&e, bound, out);
        }
    }
}

fn walk(e: &ast::Expr, bound: &mut Vec<HashSet<String>>, out: &mut HashSet<String>) {
    use ast::Expr::*;
    match e {
        Ident(i) => refer(bound, out, &ident_text(i)),
        Apply(a) => {
            if let Some(x) = a.lambda() {
                walk(&x, bound, out);
            }
            if let Some(x) = a.argument() {
                walk(&x, bound, out);
            }
        }
        Assert(a) => {
            for x in [a.condition(), a.body()].into_iter().flatten() {
                walk(&x, bound, out);
            }
        }
        IfElse(i) => {
            for x in [i.condition(), i.body(), i.else_body()].into_iter().flatten() {
                walk(&x, bound, out);
            }
        }
        Select(s) => {
            if let Some(x) = s.expr() {
                walk(&x, bound, out);
            }
            if let Some(p) = s.attrpath() {
                for a in p.attrs() {
                    walk_attr(&a, bound, out);
                }
            }
            if let Some(d) = s.default_expr() {
                walk(&d, bound, out);
            }
        }
        Str(s) => walk_str(s, bound, out),
        Literal(_) | CurPos(_) | Error(_) => {}
        PathAbs(_) | PathRel(_) | PathHome(_) => {
            let parts = match e {
                PathAbs(p) => p.parts(),
                PathRel(p) => p.parts(),
                PathHome(p) => p.parts(),
                _ => unreachable!(),
            };
            for part in parts {
                if let ast::InterpolPart::Interpolation(i) = part
                    && let Some(x) = i.expr()
                {
                    walk(&x, bound, out);
                }
            }
        }
        PathSearch(_) => {
            refer(bound, out, "__findFile");
            refer(bound, out, "__nixPath");
        }
        Lambda(l) => {
            let mut names = HashSet::new();
            match l.param() {
                Some(ast::Param::IdentParam(p)) => {
                    if let Some(i) = p.ident() {
                        names.insert(ident_text(&i));
                    }
                }
                Some(ast::Param::Pattern(p)) => {
                    for e in p.pat_entries() {
                        if let Some(i) = e.ident() {
                            names.insert(ident_text(&i));
                        }
                    }
                    if let Some(b) = p.pat_bind().and_then(|b| b.ident()) {
                        names.insert(ident_text(&b));
                    }
                }
                None => {}
            }
            bound.push(names);
            if let Some(ast::Param::Pattern(p)) = l.param() {
                for e in p.pat_entries() {
                    if let Some(d) = e.default() {
                        walk(&d, bound, out);
                    }
                }
            }
            if let Some(b) = l.body() {
                walk(&b, bound, out);
            }
            bound.pop();
        }
        LegacyLet(l) => {
            bound.push(bound_names(l));
            walk_entries(l, bound, out, true);
            bound.pop();
        }
        LetIn(l) => {
            bound.push(bound_names(l));
            walk_entries(l, bound, out, true);
            if let Some(b) = l.body() {
                walk(&b, bound, out);
            }
            bound.pop();
        }
        List(l) => {
            for x in l.items() {
                walk(&x, bound, out);
            }
        }
        BinOp(b) => {
            for x in [b.lhs(), b.rhs()].into_iter().flatten() {
                walk(&x, bound, out);
            }
        }
        Paren(p) => {
            if let Some(x) = p.expr() {
                walk(&x, bound, out);
            }
        }
        Root(r) => {
            if let Some(x) = r.expr() {
                walk(&x, bound, out);
            }
        }
        AttrSet(s) => {
            if s.rec_token().is_some() {
                bound.push(bound_names(s));
                walk_entries(s, bound, out, true);
                bound.pop();
            } else {
                walk_entries(s, bound, out, false);
            }
        }
        UnaryOp(u) => {
            if let Some(x) = u.expr() {
                walk(&x, bound, out);
            }
        }
        With(w) => {
            for x in [w.namespace(), w.body()].into_iter().flatten() {
                walk(&x, bound, out);
            }
        }
        HasAttr(h) => {
            if let Some(x) = h.expr() {
                walk(&x, bound, out);
            }
            if let Some(p) = h.attrpath() {
                for a in p.attrs() {
                    walk_attr(&a, bound, out);
                }
            }
        }
    }
}
