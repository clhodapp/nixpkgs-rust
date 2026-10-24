//! Operators, coercions and attribute selection, with the semantics of the
//! corresponding parts of Nix's evaluator (`eval.cc`).

use std::cmp::Ordering;
use std::rc::Rc;

use crate::attrs::{Attr, Attrs};
use crate::error::{Error, Kind, R};
use crate::paths::canon_path;
use crate::print::show_value_short;
use crate::string::{Context, NixStr};
use crate::symbol::{Sym, intern, known, name};
use crate::value::{Pos, Value, call, force};

/// Forces a value and returns its Boolean.
#[inline]
pub fn truthy(v: &Value) -> R<bool> {
    match force(v)? {
        Value::Bool(b) => Ok(b),
        other => Err(type_error("a Boolean", &other)),
    }
}

pub fn type_error(expected: &str, got: &Value) -> Box<Error> {
    Error::eval(format!(
        "expected {expected} but found {}: {}",
        got.show_type(),
        show_value_short(got)
    ))
}

pub fn force_attrs(v: &Value) -> R<Rc<Attrs>> {
    match force(v)? {
        Value::Attrs(a) => Ok(a),
        other => Err(type_error("a set", &other)),
    }
}

pub fn force_list(v: &Value) -> R<Rc<Vec<Value>>> {
    match force(v)? {
        Value::List(l) => Ok(l),
        other => Err(type_error("a list", &other)),
    }
}

pub fn force_int(v: &Value) -> R<i64> {
    match force(v)? {
        Value::Int(i) => Ok(i),
        other => Err(type_error("an integer", &other)),
    }
}

pub fn force_function(v: &Value) -> R<Value> {
    let f = force(v)?;
    match &f {
        Value::Lambda(_) | Value::Prim(_) => Ok(f),
        Value::Attrs(a) if a.contains(known::functor) => Ok(f),
        other => Err(type_error("a function", other)),
    }
}

/// Forces a string and returns it with its context.
pub fn force_str(v: &Value) -> R<Rc<NixStr>> {
    match force(v)? {
        Value::Str(s) => Ok(s),
        other => Err(type_error("a string", &other)),
    }
}

/// Forces a string that must not refer to the store.
pub fn force_str_no_ctx(v: &Value) -> R<Rc<NixStr>> {
    let s = force_str(v)?;
    if let Some(c) = s.ctx_iter().next() {
        return Err(Error::eval(format!(
            "the string '{}' is not allowed to refer to a store path (such as '{}')",
            s.lossy(),
            crate::context::display_elem(c)
        )));
    }
    Ok(s)
}

/// `v.k`.
pub fn select(v: &Value, k: Sym) -> R<Value> {
    let v = force(v)?;
    match &v {
        Value::Attrs(a) => match a.get(k) {
            Some(x) => Ok(x.clone()),
            None => Err(missing_attr(a, k)),
        },
        other => Err(Error::eval(format!(
            "expected a set but found {}: {}",
            other.show_type(),
            show_value_short(other)
        ))
        .with_trace(format!("while selecting attribute '{}'", name(k)))),
    }
}

fn missing_attr(a: &Attrs, k: Sym) -> Box<Error> {
    let k = name(k);
    let mut close: Vec<Rc<str>> = a
        .iter()
        .map(|x| name(x.k))
        .filter(|n| levenshtein(n, &k) <= 2)
        .collect();
    close.sort();
    if close.is_empty() {
        Error::eval(format!("attribute '{k}' missing"))
    } else {
        let list: Vec<String> = close.iter().take(10).map(|s| s.to_string()).collect();
        Error::eval(format!("attribute '{k}' missing\n       Did you mean one of {}?", list.join(", ")))
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len().abs_diff(b.len()) > 2 {
        return 3;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let c = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + c);
        }
        prev = cur;
    }
    prev[b.len()]
}

trait WithTrace {
    fn with_trace(self, t: String) -> Self;
}

impl WithTrace for Box<Error> {
    fn with_trace(mut self, t: String) -> Self {
        self.trace.push(t);
        self
    }
}

/// `v.a.b.c`.
pub fn select_path(v: &Value, path: &[Sym]) -> R<Value> {
    let mut cur = v.clone();
    for k in path {
        cur = select(&cur, *k)?;
    }
    Ok(cur)
}

/// `v.a.b.c or d`: the default is used when any step is missing or is not a
/// set.
pub fn select_or(v: &Value, path: &[Sym]) -> R<Option<Value>> {
    let mut cur = v.clone();
    for k in path {
        match force(&cur)? {
            Value::Attrs(a) => match a.get(*k) {
                Some(x) => cur = x.clone(),
                None => return Ok(None),
            },
            _ => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// `v ? a.b.c`.
pub fn has_attr_path(v: &Value, path: &[Sym]) -> R<bool> {
    let mut cur = v.clone();
    for k in path {
        match force(&cur)? {
            Value::Attrs(a) => match a.get(*k) {
                Some(x) => cur = x.clone(),
                None => return Ok(false),
            },
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Turns the value of a dynamic attribute name into a symbol. `None` means the
/// name evaluated to `null`, which drops the attribute.
pub fn dyn_attr_name(v: &Value) -> R<Option<Sym>> {
    match force(v)? {
        Value::Null => Ok(None),
        Value::Str(s) => Ok(Some(intern(&s.lossy()))),
        other => Err(type_error("a string", &other)),
    }
}

/// Like [`dyn_attr_name`] for selection, where `null` is an error.
pub fn dyn_sel_name(v: &Value) -> R<Sym> {
    let s = force_str(v)?;
    Ok(intern(&s.lossy()))
}

/// Builds the result of `a // b`.
pub fn update(a: &Value, b: &Value) -> R<Value> {
    let a = force_attrs(a)?;
    let b = force_attrs(b)?;
    if b.is_empty() {
        return Ok(Value::Attrs(a));
    }
    if a.is_empty() {
        return Ok(Value::Attrs(b));
    }
    Ok(Value::attrs(Attrs::update(&a, &b)))
}

/// `a ++ b`.
pub fn concat_lists(a: &Value, b: &Value) -> R<Value> {
    let a = force_list(a)?;
    let b = force_list(b)?;
    if b.is_empty() {
        return Ok(Value::List(a));
    }
    if a.is_empty() {
        return Ok(Value::List(b));
    }
    let mut v = Vec::with_capacity(a.len() + b.len());
    v.extend(a.iter().cloned());
    v.extend(b.iter().cloned());
    Ok(Value::list(v))
}

/// `!v`.
pub fn not(v: &Value) -> R<Value> {
    Ok(Value::Bool(!truthy(v)?))
}

/// Coerces a value to a string the way Nix's `coerceToString` does.
///
/// `coerce_more` is the mode of `toString`: Booleans, numbers, `null` and
/// lists become strings too. `copy_to_store` copies paths into the store and
/// records them in the context.
pub fn coerce_to_string(
    v: &Value,
    ctx: &mut Context,
    coerce_more: bool,
    copy_to_store: bool,
    canonicalize: bool,
) -> R<Vec<u8>> {
    let v = force(v)?;
    let _d = crate::value::depth_guard()?;
    match &v {
        Value::Str(s) => {
            if let Some(c) = &s.ctx {
                ctx.extend(c.iter().cloned());
            }
            Ok(s.s.to_vec())
        }
        Value::Path(p) => {
            if copy_to_store {
                let sp = crate::store::copy_path_to_store(p, ctx)?;
                Ok(sp.as_bytes().to_vec())
            } else {
                let _ = canonicalize;
                Ok(p.as_bytes().to_vec())
            }
        }
        Value::Attrs(a) => {
            if let Some(f) = a.get(known::toString) {
                let r = call(f, v.clone())?;
                return coerce_to_string(&r, ctx, coerce_more, copy_to_store, true);
            }
            match a.get(known::outPath) {
                Some(o) => coerce_to_string(o, ctx, coerce_more, copy_to_store, canonicalize),
                None => Err(Error::eval(format!(
                    "cannot coerce a set to a string: {}",
                    show_value_short(&v)
                ))),
            }
        }
        Value::Bool(true) if coerce_more => Ok(b"1".to_vec()),
        Value::Bool(false) | Value::Null if coerce_more => Ok(Vec::new()),
        Value::Int(i) if coerce_more => Ok(i.to_string().into_bytes()),
        Value::Float(f) if coerce_more => Ok(format!("{f:.6}").into_bytes()),
        Value::List(l) if coerce_more => {
            let mut out = Vec::new();
            for (n, x) in l.iter().enumerate() {
                out.extend(coerce_to_string(x, ctx, coerce_more, copy_to_store, canonicalize)?);
                if n + 1 < l.len() {
                    let empty_list = matches!(force(x)?, Value::List(ref y) if y.is_empty());
                    if !empty_list {
                        out.push(b' ');
                    }
                }
            }
            Ok(out)
        }
        other => Err(Error::eval(format!(
            "cannot coerce {} to a string: {}",
            other.show_type(),
            show_value_short(other)
        ))),
    }
}

/// Coerces a value to a string with its context, as `toString` does.
pub fn coerce_to_nix_str(v: &Value, coerce_more: bool, copy_to_store: bool) -> R<NixStr> {
    let mut ctx = Context::new();
    let s = coerce_to_string(v, &mut ctx, coerce_more, copy_to_store, true)?;
    Ok(NixStr::with_ctx(s, ctx))
}

/// Coerces a value to an absolute path, as Nix's `coerceToPath` does.
pub fn coerce_to_path(v: &Value) -> R<(String, Context)> {
    let fv = force(v)?;
    if let Value::Path(p) = &fv {
        return Ok((p.to_string(), Context::new()));
    }
    if let Value::Attrs(a) = &fv
        && let Some(f) = a.get(known::toString)
    {
        let r = call(f, fv.clone())?;
        return coerce_to_path(&r);
    }
    let mut ctx = Context::new();
    let s = coerce_to_string(&fv, &mut ctx, false, false, true)?;
    let s = String::from_utf8_lossy(&s).into_owned();
    if !s.starts_with('/') {
        return Err(Error::eval(format!("string '{s}' doesn't represent an absolute path")));
    }
    Ok((s, ctx))
}

/// String interpolation and `+`, which Nix implements as one operation
/// (`ExprConcatStrings`). With `force_string`, the result is a string whatever
/// the first part is; otherwise the first part decides between integer,
/// float, path and string.
pub fn concat(parts: &[Value], force_string: bool) -> R<Value> {
    #[derive(PartialEq, Clone, Copy)]
    enum T {
        Int,
        Float,
        Path,
        Str,
    }
    let mut first = !force_string;
    let mut ty = T::Str;
    let mut n: i64 = 0;
    let mut nf: f64 = 0.0;
    let mut out: Vec<u8> = Vec::new();
    let mut ctx = Context::new();
    for p in parts {
        let v = force(p)?;
        if first {
            ty = match &v {
                Value::Int(_) => T::Int,
                Value::Float(_) => T::Float,
                Value::Path(_) => T::Path,
                _ => T::Str,
            };
        }
        match ty {
            T::Int => match &v {
                Value::Int(i) => {
                    n = n
                        .checked_add(*i)
                        .ok_or_else(|| Error::eval(format!("integer overflow in adding {n} + {i}")))?
                }
                Value::Float(f) => {
                    ty = T::Float;
                    nf = n as f64 + f;
                }
                other => {
                    return Err(Error::eval(format!("cannot add {} to an integer", other.show_type())));
                }
            },
            T::Float => match &v {
                Value::Int(i) => nf += *i as f64,
                Value::Float(f) => nf += f,
                other => return Err(Error::eval(format!("cannot add {} to a float", other.show_type()))),
            },
            T::Path | T::Str => {
                let part = coerce_to_string(&v, &mut ctx, false, ty == T::Str, !first)?;
                out.extend_from_slice(&part);
            }
        }
        first = false;
    }
    Ok(match ty {
        T::Int => Value::Int(n),
        T::Float => Value::Float(nf),
        T::Path => {
            if !ctx.is_empty() {
                return Err(Error::eval(
                    "a string that refers to a store path cannot be appended to a path",
                ));
            }
            Value::Path(Rc::from(canon_path(&String::from_utf8_lossy(&out))))
        }
        T::Str => Value::string(NixStr::with_ctx(out, ctx)),
    })
}

/// `a + b`.
pub fn add(a: &Value, b: &Value) -> R<Value> {
    let (fa, fb) = (force(a)?, force(b)?);
    match (&fa, &fb) {
        (Value::Int(x), Value::Int(y)) => x
            .checked_add(*y)
            .map(Value::Int)
            .ok_or_else(|| Error::eval(format!("integer overflow in adding {x} + {y}"))),
        _ => concat(&[fa, fb], false),
    }
}

fn arith(a: &Value, b: &Value, op: &str) -> R<Value> {
    let (fa, fb) = (force(a)?, force(b)?);
    let verb = match op {
        "-" => "subtracting",
        "*" => "multiplying",
        _ => "dividing",
    };
    match (&fa, &fb) {
        (Value::Int(x), Value::Int(y)) => {
            let r = match op {
                "-" => x.checked_sub(*y),
                "*" => x.checked_mul(*y),
                _ => {
                    if *y == 0 {
                        return Err(Error::eval("division by zero"));
                    }
                    x.checked_div(*y)
                }
            };
            r.map(Value::Int)
                .ok_or_else(|| Error::eval(format!("integer overflow in {verb} {x} {op} {y}")))
        }
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            let x = as_f64(&fa);
            let y = as_f64(&fb);
            Ok(Value::Float(match op {
                "-" => x - y,
                "*" => x * y,
                _ => {
                    if y == 0.0 {
                        return Err(Error::eval("division by zero"));
                    }
                    x / y
                }
            }))
        }
        (Value::Int(_) | Value::Float(_), other) | (other, _) => Err(type_error("an integer", other)),
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => unreachable!(),
    }
}

pub fn sub(a: &Value, b: &Value) -> R<Value> {
    arith(a, b, "-")
}

pub fn mul(a: &Value, b: &Value) -> R<Value> {
    arith(a, b, "*")
}

pub fn div(a: &Value, b: &Value) -> R<Value> {
    arith(a, b, "/")
}

/// Unary minus, which Nix parses as `0 - v`.
pub fn neg(v: &Value) -> R<Value> {
    sub(&Value::Int(0), v)
}

/// `a == b`, as Nix's `eqValues`.
pub fn eq(a: &Value, b: &Value) -> R<bool> {
    eq_values(a, b, false)
}

/// Whether two values are the same object. Nix compares the elements of
/// lists and sets by address before comparing them structurally, which is
/// what makes `[ f ] == [ f ]` true for a function `f` while `f == f` is false.
fn same_object(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Thunk(x), Value::Thunk(y)) => Rc::ptr_eq(x, y),
        (Value::Lambda(x), Value::Lambda(y)) => Rc::ptr_eq(x, y),
        (Value::Prim(x), Value::Prim(y)) => Rc::ptr_eq(x, y),
        (Value::Attrs(x), Value::Attrs(y)) => Rc::ptr_eq(x, y),
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

fn eq_values(a: &Value, b: &Value, nested: bool) -> R<bool> {
    if nested && same_object(a, b) {
        return Ok(true);
    }
    let a = force(a)?;
    let b = force(b)?;
    if nested && same_object(&a, &b) {
        return Ok(true);
    }
    Ok(match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Int(x), Value::Float(y)) => (*x as f64) == *y,
        (Value::Float(x), Value::Int(y)) => *x == (*y as f64),
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x.s == y.s,
        (Value::Path(x), Value::Path(y)) => x == y,
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::List(x), Value::List(y)) => {
            if x.len() != y.len() {
                return Ok(false);
            }
            for (p, q) in x.iter().zip(y.iter()) {
                if !eq_values(p, q, true)? {
                    return Ok(false);
                }
            }
            true
        }
        (Value::Attrs(x), Value::Attrs(y)) => {
            if is_derivation(x)? && is_derivation(y)? {
                if let (Some(o1), Some(o2)) = (x.get(known::outPath), y.get(known::outPath)) {
                    return eq_values(o1, o2, true);
                }
            }
            if x.len() != y.len() {
                return Ok(false);
            }
            for (p, q) in x.iter().zip(y.iter()) {
                if p.k != q.k || !eq_values(&p.v, &q.v, true)? {
                    return Ok(false);
                }
            }
            true
        }
        (Value::Lambda(_) | Value::Prim(_), _) | (_, Value::Lambda(_) | Value::Prim(_)) => false,
        _ => false,
    })
}

/// Whether a set has `type = "derivation"`.
pub fn is_derivation(a: &Attrs) -> R<bool> {
    match a.get(known::type_) {
        Some(t) => match force(t)? {
            Value::Str(s) => Ok(&*s.s == b"derivation"),
            _ => Ok(false),
        },
        None => Ok(false),
    }
}

/// `a < b`, as Nix's `CompareValues`.
pub fn less_than(a: &Value, b: &Value) -> R<bool> {
    Ok(compare(a, b)? == Ordering::Less)
}

pub fn compare(a: &Value, b: &Value) -> R<Ordering> {
    let a = force(a)?;
    let b = force(b)?;
    Ok(match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            let (x, y) = (as_f64(&a), as_f64(&b));
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
        (Value::Str(x), Value::Str(y)) => x.s.cmp(&y.s),
        (Value::Path(x), Value::Path(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::List(x), Value::List(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                if !eq_values(p, q, true)? {
                    return compare(p, q);
                }
            }
            x.len().cmp(&y.len())
        }
        _ => {
            return Err(Error::eval(format!(
                "cannot compare {} with {}",
                a.show_type(),
                b.show_type()
            )));
        }
    })
}

/// `assert c; e` when `c` is false.
pub fn assertion_failed(text: &str) -> Box<Error> {
    Error::new(Kind::Assert, format!("assertion '{text}' failed"))
}

/// Looks a name up in the namespaces of the enclosing `with` expressions,
/// innermost first. The compiler emits this only for names no `let`,
/// function argument or recursive set binds.
pub fn with_lookup(scopes: &[&Value], k: Sym) -> R<Value> {
    for s in scopes {
        let a = force_attrs(s)?;
        if let Some(v) = a.get(k) {
            return Ok(v.clone());
        }
    }
    Err(Error::eval(format!("undefined variable '{}'", name(k))))
}

/// Rejects arguments a function written with a `{ ... }` pattern without an
/// ellipsis does not take.
pub fn check_formals(args: &Attrs, info: &'static crate::value::LambdaInfo) -> R<()> {
    let Some(f) = &info.formals else { return Ok(()) };
    if f.ellipsis {
        return Ok(());
    }
    for a in args.iter() {
        if f.names.binary_search_by_key(&a.k, |(k, _, _)| *k).is_err() {
            let mut expected: Vec<String> = f.names.iter().map(|(k, _, _)| name(*k).to_string()).collect();
            expected.sort();
            return Err(Error::eval(format!(
                "function '{}' called with unexpected argument '{}'\n       Did you mean one of {}?",
                lambda_name(info),
                name(a.k),
                expected.join(", ")
            )));
        }
    }
    Ok(())
}

fn lambda_name(_info: &crate::value::LambdaInfo) -> String {
    "anonymous lambda".to_string()
}

/// The error for a required argument the caller did not pass.
pub fn formal_missing(k: Sym, info: &'static crate::value::LambdaInfo) -> Box<Error> {
    Error::eval(format!(
        "function '{}' called without required argument '{}'",
        lambda_name(info),
        name(k)
    ))
}

/// The value a recursive set with `__overrides` gives one of its names: the
/// override when `__overrides` has the name, the set's own value otherwise.
pub fn override_or(overrides: Value, k: Sym, own: Value) -> Value {
    Value::native(move || match force_attrs(&overrides)?.get(k) {
        Some(v) => Ok(v.clone()),
        None => Ok(own.clone()),
    })
}

/// Applies the `__overrides` attribute of a recursive set to the set.
pub fn apply_overrides(v: Value) -> R<Value> {
    let a = force_attrs(&v)?;
    match a.get(intern("__overrides")) {
        Some(o) => {
            let o = force_attrs(o)?;
            Ok(Value::attrs(Attrs::update(&a, &o)))
        }
        None => Ok(v),
    }
}

/// Builds a static attribute set from entries the compiler sorted by id.
pub fn attrs_static(e: Vec<Attr>) -> Value {
    Value::attrs(Attrs::from_sorted(e))
}

/// Adds a dynamic attribute (`${k} = v;`) to a set under construction.
pub fn insert_dynamic(a: &mut Attrs, k: &Value, v: Value, pos: Pos) -> R<()> {
    if let Some(k) = dyn_attr_name(k)? {
        if !a.insert_new(Attr { k, v, pos }) {
            return Err(Error::eval(format!("dynamic attribute '{}' already defined", name(k))));
        }
    }
    Ok(())
}
