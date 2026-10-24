//! The built-in functions and constants, and the global environment.

use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use crate::attrs::{Attr, Attrs};
use crate::error::{Context as _, Error, Kind, R};
use crate::json::{parse_json, value_to_json};
use crate::ops::{
    self, coerce_to_nix_str, coerce_to_path, coerce_to_string, compare, eq, force_attrs, force_function, force_int,
    force_list, force_str, force_str_no_ctx, truthy, type_error,
};
use crate::print::{print_strict, show_value_short};
use crate::store::{self, Algo, Hash, HashFormat, Method, STATE};
use crate::string::{Context, CtxElem, NixStr};
use crate::symbol::{Sym, intern, known, name};
use crate::value::{Pos, PrimOp, Value, call, call_n, deep_force, force, pos_info};

fn attr(k: Sym, v: Value) -> Attr {
    Attr {
        k,
        v,
        pos: Pos::default(),
    }
}

fn str_val(s: &str) -> Value {
    Value::str(s.as_bytes())
}

fn bytes_of(v: &Value) -> R<Vec<u8>> {
    Ok(force_str(v)?.s.to_vec())
}

fn string_of(v: &Value) -> R<String> {
    Ok(force_str(v)?.lossy().into_owned())
}

fn force_float(v: &Value) -> R<f64> {
    match force(v)? {
        Value::Float(f) => Ok(f),
        Value::Int(i) => Ok(i as f64),
        other => Err(type_error("a float", &other)),
    }
}

// ---------------------------------------------------------------------------
// Types

fn p_type_of(a: &[Value]) -> R<Value> {
    Ok(str_val(force(&a[0])?.type_name()))
}

macro_rules! is_type {
    ($fname:ident, $pat:pat) => {
        fn $fname(a: &[Value]) -> R<Value> {
            Ok(Value::Bool(matches!(force(&a[0])?, $pat)))
        }
    };
}

is_type!(p_is_attrs, Value::Attrs(_));
is_type!(p_is_bool, Value::Bool(_));
is_type!(p_is_float, Value::Float(_));
is_type!(p_is_function, Value::Lambda(_) | Value::Prim(_));
is_type!(p_is_int, Value::Int(_));
is_type!(p_is_list, Value::List(_));
is_type!(p_is_null, Value::Null);
is_type!(p_is_path, Value::Path(_));
is_type!(p_is_string, Value::Str(_));

fn p_function_args(a: &[Value]) -> R<Value> {
    match force(&a[0])? {
        Value::Lambda(l) => {
            let mut e = Vec::new();
            if let Some(f) = &l.info.formals {
                for (k, d, pos) in f.names {
                    e.push(Attr {
                        k: *k,
                        v: Value::Bool(*d),
                        pos: *pos,
                    });
                }
            }
            Ok(Value::attrs(Attrs::from_vec_first_wins(e)))
        }
        Value::Prim(_) => Ok(Value::attrs(Attrs::new())),
        other => Err(type_error("a function", &other)),
    }
}

// ---------------------------------------------------------------------------
// Arithmetic and comparison

fn p_add(a: &[Value]) -> R<Value> {
    let (x, y) = (force(&a[0])?, force(&a[1])?);
    match (&x, &y) {
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => ops::add(&x, &y),
        (Value::Int(_) | Value::Float(_), o) | (o, _) => Err(type_error("an integer", o)),
    }
}

fn p_sub(a: &[Value]) -> R<Value> {
    ops::sub(&a[0], &a[1])
}

fn p_mul(a: &[Value]) -> R<Value> {
    ops::mul(&a[0], &a[1])
}

fn p_div(a: &[Value]) -> R<Value> {
    ops::div(&a[0], &a[1])
}

fn p_less_than(a: &[Value]) -> R<Value> {
    Ok(Value::Bool(ops::less_than(&a[0], &a[1])?))
}

fn p_bit_and(a: &[Value]) -> R<Value> {
    Ok(Value::Int(force_int(&a[0])? & force_int(&a[1])?))
}

fn p_bit_or(a: &[Value]) -> R<Value> {
    Ok(Value::Int(force_int(&a[0])? | force_int(&a[1])?))
}

fn p_bit_xor(a: &[Value]) -> R<Value> {
    Ok(Value::Int(force_int(&a[0])? ^ force_int(&a[1])?))
}

fn float_to_int(f: f64, what: &str) -> R<Value> {
    if f.is_nan() || f < -9.223372036854776e18 || f >= 9.223372036854776e18 {
        return Err(Error::eval(format!(
            "float {} is not in the range of representable integers, can't {what}",
            crate::print::format_float(f)
        )));
    }
    Ok(Value::Int(f as i64))
}

fn p_ceil(a: &[Value]) -> R<Value> {
    match force(&a[0])? {
        Value::Int(i) => Ok(Value::Int(i)),
        _ => float_to_int(force_float(&a[0])?.ceil(), "ceil"),
    }
}

fn p_floor(a: &[Value]) -> R<Value> {
    match force(&a[0])? {
        Value::Int(i) => Ok(Value::Int(i)),
        _ => float_to_int(force_float(&a[0])?.floor(), "floor"),
    }
}

// ---------------------------------------------------------------------------
// Attribute sets

fn p_attr_names(a: &[Value]) -> R<Value> {
    let s = force_attrs(&a[0])?;
    Ok(Value::list(s.by_name().into_iter().map(|at| str_val(&name(at.k))).collect()))
}

fn p_attr_values(a: &[Value]) -> R<Value> {
    let s = force_attrs(&a[0])?;
    Ok(Value::list(s.by_name().into_iter().map(|at| at.v.clone()).collect()))
}

fn p_get_attr(a: &[Value]) -> R<Value> {
    let k = intern(&string_of(&a[0])?);
    ops::select(&a[1], k)
}

fn p_has_attr(a: &[Value]) -> R<Value> {
    let k = intern(&string_of(&a[0])?);
    Ok(Value::Bool(force_attrs(&a[1])?.contains(k)))
}

fn p_remove_attrs(a: &[Value]) -> R<Value> {
    let s = force_attrs(&a[0])?;
    let names = force_list(&a[1])?;
    let mut out = (*s).clone();
    for n in names.iter() {
        out.remove(intern(&string_of(n)?));
    }
    Ok(Value::attrs(out))
}

fn p_intersect_attrs(a: &[Value]) -> R<Value> {
    let x = force_attrs(&a[0])?;
    let y = force_attrs(&a[1])?;
    // Walk the smaller set: callPackage intersects a few formals with all
    // of nixpkgs. Both orders yield the entries sorted by symbol.
    let e: Vec<Attr> = if x.len() < y.len() {
        x.iter().filter_map(|at| y.get_attr(at.k).cloned()).collect()
    } else {
        y.iter().filter(|at| x.contains(at.k)).cloned().collect()
    };
    Ok(Value::attrs(Attrs::from_sorted(e)))
}

fn p_cat_attrs(a: &[Value]) -> R<Value> {
    let k = intern(&string_of(&a[0])?);
    let l = force_list(&a[1])?;
    let mut out = Vec::new();
    for x in l.iter() {
        if let Some(v) = force_attrs(x)?.get(k) {
            out.push(v.clone());
        }
    }
    Ok(Value::list(out))
}

fn p_list_to_attrs(a: &[Value]) -> R<Value> {
    let l = force_list(&a[0])?;
    let mut e = Vec::with_capacity(l.len());
    for x in l.iter() {
        let s = force_attrs(x)?;
        let n = s
            .get(known::name)
            .ok_or_else(|| Error::eval("attribute 'name' missing"))
            .and_then(string_of)
            .context(|| "while evaluating the `name` attribute of an element of the list passed to builtins.listToAttrs".to_string())?;
        let v = s
            .get_attr(known::value)
            .ok_or_else(|| Error::eval("attribute 'value' missing"))?;
        e.push(Attr {
            k: intern(&n),
            v: v.v.clone(),
            pos: v.pos,
        });
    }
    Ok(Value::attrs(Attrs::from_vec_first_wins(e)))
}

fn p_map_attrs(a: &[Value]) -> R<Value> {
    let f = a[0].clone();
    let s = force_attrs(&a[1])?;
    let e = s
        .iter()
        .map(|at| {
            let (f, n, v) = (f.clone(), str_val(&name(at.k)), at.v.clone());
            Attr {
                k: at.k,
                v: Value::native(move || call_n(&f, &[n.clone(), v.clone()])),
                pos: at.pos,
            }
        })
        .collect();
    Ok(Value::attrs(Attrs::from_sorted(e)))
}

fn p_zip_attrs_with(a: &[Value]) -> R<Value> {
    let f = a[0].clone();
    let l = force_list(&a[1])?;
    let mut acc: std::collections::BTreeMap<Sym, Vec<Value>> = Default::default();
    for x in l.iter() {
        for at in force_attrs(x)?.iter() {
            acc.entry(at.k).or_default().push(at.v.clone());
        }
    }
    let e = acc
        .into_iter()
        .map(|(k, vs)| {
            let (f, n, vs) = (f.clone(), str_val(&name(k)), Value::list(vs));
            attr(k, Value::native(move || call_n(&f, &[n.clone(), vs.clone()])))
        })
        .collect();
    Ok(Value::attrs(Attrs::from_sorted(e)))
}

fn p_unsafe_get_attr_pos(a: &[Value]) -> R<Value> {
    let k = intern(&string_of(&a[0])?);
    let s = force_attrs(&a[1])?;
    Ok(match s.get_attr(k) {
        Some(at) => pos_value(at.pos),
        None => Value::Null,
    })
}

/// The `{ column, file, line }` set Nix uses for a position, or `null`.
pub fn pos_value(p: Pos) -> Value {
    match pos_info(p) {
        Some(info) => Value::attrs(Attrs::from_vec_first_wins(vec![
            attr(known::column, Value::Int(info.column as i64)),
            attr(known::file, str_val(&crate::program::file_path(info.file))),
            attr(known::line, Value::Int(info.line as i64)),
        ])),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Lists

fn p_length(a: &[Value]) -> R<Value> {
    Ok(Value::Int(force_list(&a[0])?.len() as i64))
}

fn p_head(a: &[Value]) -> R<Value> {
    force_list(&a[0])?
        .first()
        .cloned()
        .ok_or_else(|| Error::eval("'builtins.head' called on an empty list"))
}

fn p_tail(a: &[Value]) -> R<Value> {
    let l = force_list(&a[0])?;
    if l.is_empty() {
        return Err(Error::eval("'builtins.tail' called on an empty list"));
    }
    Ok(Value::list(l[1..].to_vec()))
}

fn p_elem_at(a: &[Value]) -> R<Value> {
    let l = force_list(&a[0])?;
    let i = force_int(&a[1])?;
    if i < 0 || i as usize >= l.len() {
        return Err(Error::eval(format!(
            "'builtins.elemAt' called with index {i} on a list of size {}",
            l.len()
        )));
    }
    Ok(l[i as usize].clone())
}

fn p_elem(a: &[Value]) -> R<Value> {
    for x in force_list(&a[1])?.iter() {
        if eq(&a[0], x)? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn p_map(a: &[Value]) -> R<Value> {
    let l = force_list(&a[1])?;
    Ok(Value::list(l.iter().map(|x| Value::app(a[0].clone(), x.clone())).collect()))
}

fn p_filter(a: &[Value]) -> R<Value> {
    let l = force_list(&a[1])?;
    let f = force_function(&a[0])?;
    let mut out = Vec::new();
    for x in l.iter() {
        if truthy(&call(&f, x.clone())?)? {
            out.push(x.clone());
        }
    }
    if out.len() == l.len() {
        return Ok(Value::List(l));
    }
    Ok(Value::list(out))
}

fn p_concat_lists(a: &[Value]) -> R<Value> {
    let l = force_list(&a[0])?;
    let mut out = Vec::new();
    for x in l.iter() {
        out.extend(force_list(x)?.iter().cloned());
    }
    Ok(Value::list(out))
}

fn p_concat_map(a: &[Value]) -> R<Value> {
    let l = force_list(&a[1])?;
    let f = force_function(&a[0])?;
    let mut out = Vec::new();
    for x in l.iter() {
        out.extend(force_list(&call(&f, x.clone())?)?.iter().cloned());
    }
    Ok(Value::list(out))
}

fn p_foldl(a: &[Value]) -> R<Value> {
    let f = a[0].clone();
    let mut acc = a[1].clone();
    let l = force_list(&a[2])?;
    if l.is_empty() {
        return Ok(acc);
    }
    let f = force_function(&f)?;
    for x in l.iter() {
        acc = force(&call_n(&f, &[acc, x.clone()])?)?;
    }
    Ok(acc)
}

fn p_any(a: &[Value]) -> R<Value> {
    let f = force_function(&a[0])?;
    for x in force_list(&a[1])?.iter() {
        if truthy(&call(&f, x.clone())?)? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn p_all(a: &[Value]) -> R<Value> {
    let f = force_function(&a[0])?;
    for x in force_list(&a[1])?.iter() {
        if !truthy(&call(&f, x.clone())?)? {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn p_gen_list(a: &[Value]) -> R<Value> {
    let n = force_int(&a[1])?;
    if n < 0 {
        return Err(Error::eval(format!("cannot create list of size {n}")));
    }
    let f = force_function(&a[0])?;
    Ok(Value::list((0..n).map(|i| Value::app(f.clone(), Value::Int(i))).collect()))
}

fn p_partition(a: &[Value]) -> R<Value> {
    let f = force_function(&a[0])?;
    let (mut right, mut wrong) = (Vec::new(), Vec::new());
    for x in force_list(&a[1])?.iter() {
        if truthy(&call(&f, x.clone())?)? {
            right.push(x.clone());
        } else {
            wrong.push(x.clone());
        }
    }
    Ok(Value::attrs(Attrs::from_vec_first_wins(vec![
        attr(known::right, Value::list(right)),
        attr(known::wrong, Value::list(wrong)),
    ])))
}

fn p_group_by(a: &[Value]) -> R<Value> {
    let f = force_function(&a[0])?;
    let mut groups: std::collections::HashMap<Sym, Vec<Value>> = Default::default();
    let mut order = Vec::new();
    for x in force_list(&a[1])?.iter() {
        let k = intern(&string_of(&call(&f, x.clone())?)?);
        groups
            .entry(k)
            .or_insert_with(|| {
                order.push(k);
                Vec::new()
            })
            .push(x.clone());
    }
    let e = order
        .into_iter()
        .map(|k| attr(k, Value::list(groups.remove(&k).unwrap())))
        .collect();
    Ok(Value::attrs(Attrs::from_vec_first_wins(e)))
}

/// A stable merge sort whose comparison can fail.
fn merge_sort(v: Vec<Value>, less: &mut dyn FnMut(&Value, &Value) -> R<bool>) -> R<Vec<Value>> {
    if v.len() <= 1 {
        return Ok(v);
    }
    let mut v = v;
    let right = v.split_off(v.len() / 2);
    let left = merge_sort(v, less)?;
    let right = merge_sort(right, less)?;
    let mut out = Vec::with_capacity(left.len() + right.len());
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        if less(&right[j], &left[i])? {
            out.push(right[j].clone());
            j += 1;
        } else {
            out.push(left[i].clone());
            i += 1;
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    Ok(out)
}

fn p_sort(a: &[Value]) -> R<Value> {
    let l = force_list(&a[1])?;
    let f = force_function(&a[0])?;
    for x in l.iter() {
        force(x)?;
    }
    let sorted = merge_sort(l.to_vec(), &mut |x, y| truthy(&call_n(&f, &[x.clone(), y.clone()])?))?;
    Ok(Value::list(sorted))
}

fn p_generic_closure(a: &[Value]) -> R<Value> {
    let args = force_attrs(&a[0])?;
    let start = args
        .get(known::startSet)
        .ok_or_else(|| Error::eval("attribute 'startSet' missing"))?;
    let op = args
        .get(known::operator)
        .ok_or_else(|| Error::eval("attribute 'operator' missing"))?
        .clone();
    let mut work: std::collections::VecDeque<Value> = force_list(start)?.iter().cloned().collect();
    let mut done: Vec<Value> = Vec::new();
    let mut res = Vec::new();
    while let Some(e) = work.pop_front() {
        let s = force_attrs(&e)?;
        let key = s
            .get(known::key)
            .ok_or_else(|| Error::eval("attribute 'key' required"))?;
        let key = force(key)?;
        let mut err = None;
        let pos = done.binary_search_by(|k| match compare(k, &key) {
            Ok(o) => o,
            Err(e) => {
                err = Some(e);
                std::cmp::Ordering::Equal
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        match pos {
            Ok(_) => continue,
            Err(i) => done.insert(i, key),
        }
        res.push(e.clone());
        for x in force_list(&call(&op, e)?)?.iter() {
            work.push_back(x.clone());
        }
    }
    Ok(Value::list(res))
}

// ---------------------------------------------------------------------------
// Strings

fn p_string_length(a: &[Value]) -> R<Value> {
    let mut ctx = Context::new();
    Ok(Value::Int(coerce_to_string(&a[0], &mut ctx, false, true, true)?.len() as i64))
}

fn p_substring(a: &[Value]) -> R<Value> {
    let start = force_int(&a[0])?;
    let len = force_int(&a[1])?;
    let mut ctx = Context::new();
    let s = coerce_to_string(&a[2], &mut ctx, false, true, true)?;
    if start < 0 {
        return Err(Error::eval("negative start position in 'substring'"));
    }
    let start = (start as usize).min(s.len());
    let end = if len < 0 {
        s.len()
    } else {
        start.saturating_add(len as usize).min(s.len())
    };
    Ok(Value::string(NixStr::with_ctx(s[start..end].to_vec(), ctx)))
}

fn p_to_string(a: &[Value]) -> R<Value> {
    Ok(Value::string(coerce_to_nix_str(&a[0], true, false)?))
}

fn p_concat_strings_sep(a: &[Value]) -> R<Value> {
    let sep = force_str(&a[0])?;
    let l = force_list(&a[1])?;
    let mut ctx = Context::new();
    ctx.extend(sep.ctx_iter().cloned());
    let mut out = Vec::new();
    for (n, x) in l.iter().enumerate() {
        if n > 0 {
            out.extend_from_slice(&sep.s);
        }
        out.extend(coerce_to_string(x, &mut ctx, false, true, true)?);
    }
    Ok(Value::string(NixStr::with_ctx(out, ctx)))
}

fn p_replace_strings(a: &[Value]) -> R<Value> {
    let from = force_list(&a[0])?;
    let to = force_list(&a[1])?;
    if from.len() != to.len() {
        return Err(Error::eval(
            "'from' and 'to' arguments passed to builtins.replaceStrings have different lengths",
        ));
    }
    let from: Vec<Vec<u8>> = from.iter().map(bytes_of).collect::<R<_>>()?;
    let mut to_cache: Vec<Option<Rc<NixStr>>> = vec![None; to.len()];
    let s = force_str(&a[2])?;
    let mut ctx: Context = s.ctx_iter().cloned().collect();
    let b = &s.s;
    let mut out = Vec::with_capacity(b.len());
    let mut p = 0;
    while p <= b.len() {
        let mut found = false;
        for (i, f) in from.iter().enumerate() {
            if b[p..].starts_with(f) {
                found = true;
                let t = match &to_cache[i] {
                    Some(t) => t.clone(),
                    None => {
                        let t = force_str(&to[i])?;
                        ctx.extend(t.ctx_iter().cloned());
                        to_cache[i] = Some(t.clone());
                        t
                    }
                };
                out.extend_from_slice(&t.s);
                if f.is_empty() {
                    if p < b.len() {
                        out.push(b[p]);
                    }
                    p += 1;
                } else {
                    p += f.len();
                }
                break;
            }
        }
        if !found {
            if p < b.len() {
                out.push(b[p]);
            }
            p += 1;
        }
    }
    Ok(Value::string(NixStr::with_ctx(out, ctx)))
}

fn p_match(a: &[Value]) -> R<Value> {
    let re = crate::regex::compile(&string_of(&a[0])?)?;
    let s = force_str(&a[1])?;
    Ok(match re.match_full(&s.s) {
        Some(groups) => Value::list(
            groups
                .into_iter()
                .map(|g| g.map(Value::str).unwrap_or(Value::Null))
                .collect(),
        ),
        None => Value::Null,
    })
}

fn p_split(a: &[Value]) -> R<Value> {
    let re = crate::regex::compile(&string_of(&a[0])?)?;
    let s = force_str(&a[1])?;
    let parts = re.split(&s.s);
    Ok(Value::list(
        parts
            .into_iter()
            .map(|p| match p {
                Ok(t) => Value::str(t),
                Err(groups) => Value::list(
                    groups
                        .into_iter()
                        .map(|g| g.map(Value::str).unwrap_or(Value::Null))
                        .collect(),
                ),
            })
            .collect(),
    ))
}

fn version_components(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] == b'.' || b[i] == b'-') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        if b[i].is_ascii_digit() {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        } else {
            while i < b.len() && !(b[i].is_ascii_digit() || b[i] == b'.' || b[i] == b'-') {
                i += 1;
            }
        }
        out.push(&s[start..i]);
    }
    out
}

fn component_lt(c1: &str, c2: &str) -> bool {
    let n1 = c1.parse::<i64>().ok().filter(|_| !c1.is_empty());
    let n2 = c2.parse::<i64>().ok().filter(|_| !c2.is_empty());
    match (n1, n2) {
        (Some(a), Some(b)) => a < b,
        _ if c1.is_empty() && n2.is_some() => true,
        _ if c1 == "pre" && c2 != "pre" => true,
        _ if c2 == "pre" => false,
        (_, Some(_)) => true,
        (Some(_), _) => false,
        _ => c1 < c2,
    }
}

/// `builtins.compareVersions`.
pub fn compare_versions(v1: &str, v2: &str) -> i64 {
    let a = version_components(v1);
    let b = version_components(v2);
    for i in 0..a.len().max(b.len()) {
        let c1 = a.get(i).copied().unwrap_or("");
        let c2 = b.get(i).copied().unwrap_or("");
        if component_lt(c1, c2) {
            return -1;
        }
        if component_lt(c2, c1) {
            return 1;
        }
    }
    0
}

fn p_compare_versions(a: &[Value]) -> R<Value> {
    Ok(Value::Int(compare_versions(&string_of(&a[0])?, &string_of(&a[1])?)))
}

fn p_split_version(a: &[Value]) -> R<Value> {
    let s = string_of(&a[0])?;
    Ok(Value::list(version_components(&s).into_iter().map(str_val).collect()))
}

fn p_parse_drv_name(a: &[Value]) -> R<Value> {
    let s = string_of(&a[0])?;
    let b = s.as_bytes();
    let mut name_part = s.as_str();
    let mut version = "";
    for i in 0..b.len() {
        if b[i] == b'-' && i + 1 < b.len() && !b[i + 1].is_ascii_alphabetic() {
            name_part = &s[..i];
            version = &s[i + 1..];
            break;
        }
    }
    Ok(Value::attrs(Attrs::from_vec_first_wins(vec![
        attr(known::name, str_val(name_part)),
        attr(known::version, str_val(version)),
    ])))
}

fn p_base_name_of(a: &[Value]) -> R<Value> {
    let mut ctx = Context::new();
    let s = coerce_to_string(&a[0], &mut ctx, false, false, true)?;
    let s = String::from_utf8_lossy(&s).into_owned();
    Ok(Value::string(NixStr::with_ctx(
        crate::paths::base_name_of(&s).as_bytes().to_vec(),
        ctx,
    )))
}

fn p_dir_of(a: &[Value]) -> R<Value> {
    match force(&a[0])? {
        Value::Path(p) => Ok(Value::Path(Rc::from(crate::paths::dir_of(&p)))),
        _ => {
            let mut ctx = Context::new();
            let s = coerce_to_string(&a[0], &mut ctx, false, false, true)?;
            let s = String::from_utf8_lossy(&s).into_owned();
            Ok(Value::string(NixStr::with_ctx(crate::paths::dir_of(&s).into_bytes(), ctx)))
        }
    }
}

// ---------------------------------------------------------------------------
// Hashes

fn parse_algo(v: &Value) -> R<Algo> {
    let s = string_of(v)?;
    Algo::parse(&s).ok_or_else(|| Error::eval(format!("unknown hash algorithm '{s}'")))
}

fn p_hash_string(a: &[Value]) -> R<Value> {
    let algo = parse_algo(&a[0])?;
    let s = force_str(&a[1])?;
    Ok(str_val(&Hash::of(algo, &s.s).hex()))
}

fn p_hash_file(a: &[Value]) -> R<Value> {
    let algo = parse_algo(&a[0])?;
    let (p, _) = coerce_to_path(&a[1])?;
    let data = std::fs::read(&p).map_err(|e| Error::eval(format!("cannot read '{p}': {e}")))?;
    Ok(str_val(&Hash::of(algo, &data).hex()))
}

fn p_convert_hash(a: &[Value]) -> R<Value> {
    let args = force_attrs(&a[0])?;
    let h = args
        .get(known::hash)
        .ok_or_else(|| Error::eval("attribute 'hash' missing"))
        .and_then(string_of)?;
    let algo = match args.get(intern("hashAlgo")) {
        Some(v) => Some(parse_algo(v)?),
        None => None,
    };
    let fmt = args
        .get(intern("toHashFormat"))
        .ok_or_else(|| Error::eval("attribute 'toHashFormat' missing"))
        .and_then(string_of)?;
    let hash = Hash::parse(&h, algo)?;
    let out = match fmt.as_str() {
        "base16" => hash.format(HashFormat::Base16, false),
        "nix32" | "base32" => hash.format(HashFormat::Nix32, false),
        "base64" => hash.format(HashFormat::Base64, false),
        "sri" => hash.format(HashFormat::Sri, false),
        other => return Err(Error::eval(format!("unknown hash format '{other}'"))),
    };
    Ok(str_val(&out))
}

// ---------------------------------------------------------------------------
// Control

fn p_seq(a: &[Value]) -> R<Value> {
    force(&a[0])?;
    Ok(a[1].clone())
}

fn p_deep_seq(a: &[Value]) -> R<Value> {
    deep_force(&a[0])?;
    Ok(a[1].clone())
}

fn p_throw(a: &[Value]) -> R<Value> {
    let s = coerce_to_nix_str(&a[0], false, false)?;
    Err(Error::new(Kind::Throw, s.lossy().into_owned()))
}

fn p_abort(a: &[Value]) -> R<Value> {
    let s = coerce_to_nix_str(&a[0], false, false)?;
    Err(Error::new(Kind::Abort, s.lossy().into_owned()))
}

fn p_try_eval(a: &[Value]) -> R<Value> {
    let (ok, v) = match force(&a[0]) {
        Ok(v) => (true, v),
        Err(e) if e.catchable() => (false, Value::Bool(false)),
        Err(e) => return Err(e),
    };
    Ok(Value::attrs(Attrs::from_vec_first_wins(vec![
        attr(known::success, Value::Bool(ok)),
        attr(known::value, v),
    ])))
}

fn p_add_error_context(a: &[Value]) -> R<Value> {
    match force(&a[1]) {
        Ok(v) => Ok(v),
        Err(mut e) => {
            if let Ok(s) = force_str(&a[0]) {
                e.trace.push(s.lossy().into_owned());
            }
            Err(e)
        }
    }
}

fn p_trace(a: &[Value]) -> R<Value> {
    let v = force(&a[0])?;
    match &v {
        Value::Str(s) => eprintln!("trace: {}", s.lossy()),
        other => eprintln!("trace: {}", show_value_short(other)),
    }
    Ok(a[1].clone())
}

fn p_trace_verbose(a: &[Value]) -> R<Value> {
    Ok(a[1].clone())
}

fn p_warn(a: &[Value]) -> R<Value> {
    let s = force_str(&a[0])?;
    eprintln!("evaluation warning: {}", s.lossy());
    Ok(a[1].clone())
}

fn p_break(a: &[Value]) -> R<Value> {
    Ok(a[0].clone())
}

// ---------------------------------------------------------------------------
// Context

fn p_unsafe_discard_string_context(a: &[Value]) -> R<Value> {
    let s = coerce_to_nix_str(&a[0], false, false)?;
    Ok(Value::str(s.s))
}

fn p_has_context(a: &[Value]) -> R<Value> {
    Ok(Value::Bool(force_str(&a[0])?.has_ctx()))
}

fn p_unsafe_discard_output_dependency(a: &[Value]) -> R<Value> {
    let s = force_str(&a[0])?;
    let ctx: Context = s
        .ctx_iter()
        .map(|c| match c {
            CtxElem::DrvDeep(d) => CtxElem::Opaque(d.clone()),
            other => other.clone(),
        })
        .collect();
    Ok(Value::string(NixStr::with_ctx(s.s.clone(), ctx)))
}

fn p_add_drv_output_dependencies(a: &[Value]) -> R<Value> {
    let s = force_str(&a[0])?;
    let elems: Vec<&CtxElem> = s.ctx_iter().collect();
    if elems.len() != 1 {
        return Err(Error::eval(format!(
            "context of string '{}' must have exactly one element, but has {}",
            s.lossy(),
            elems.len()
        )));
    }
    let new = match elems[0] {
        CtxElem::Opaque(p) if p.ends_with(".drv") => CtxElem::DrvDeep(p.clone()),
        CtxElem::Opaque(p) => {
            return Err(Error::eval(format!(
                "path '{p}' is not a derivation, so cannot add its output dependencies"
            )));
        }
        CtxElem::DrvDeep(_) => elems[0].clone(),
        CtxElem::Built(..) => {
            return Err(Error::eval(
                "`addDrvOutputDependencies` can only act on derivations, not on a derivation output",
            ));
        }
    };
    Ok(Value::string(NixStr::with_ctx(s.s.clone(), [new].into_iter().collect())))
}

fn p_get_context(a: &[Value]) -> R<Value> {
    let s = force_str(&a[0])?;
    #[derive(Default)]
    struct Info {
        path: bool,
        all: bool,
        outputs: Vec<Rc<str>>,
    }
    let mut m: std::collections::BTreeMap<Rc<str>, Info> = Default::default();
    for c in s.ctx_iter() {
        match c {
            CtxElem::Opaque(p) => m.entry(p.clone()).or_default().path = true,
            CtxElem::DrvDeep(p) => m.entry(p.clone()).or_default().all = true,
            CtxElem::Built(d, o) => m.entry(d.clone()).or_default().outputs.push(o.clone()),
        }
    }
    let e = m
        .into_iter()
        .map(|(p, info)| {
            let mut fields = Vec::new();
            if info.all {
                fields.push(attr(intern("allOutputs"), Value::Bool(true)));
            }
            if !info.outputs.is_empty() {
                fields.push(attr(
                    known::outputs,
                    Value::list(info.outputs.iter().map(|o| str_val(o)).collect()),
                ));
            }
            if info.path {
                fields.push(attr(known::path, Value::Bool(true)));
            }
            attr(intern(&p), Value::attrs(Attrs::from_vec_first_wins(fields)))
        })
        .collect();
    Ok(Value::attrs(Attrs::from_vec_first_wins(e)))
}

fn p_append_context(a: &[Value]) -> R<Value> {
    let s = force_str(&a[0])?;
    let mut ctx: Context = s.ctx_iter().cloned().collect();
    let extra = force_attrs(&a[1])?;
    for at in extra.by_name() {
        let p: Rc<str> = name(at.k);
        if !store::is_store_path(&p) {
            return Err(Error::eval(format!(
                "context key '{p}' is not a store path"
            )));
        }
        let info = force_attrs(&at.v)?;
        if let Some(v) = info.get(known::path)
            && truthy(v)?
        {
            ctx.insert(CtxElem::Opaque(p.clone()));
        }
        if let Some(v) = info.get(intern("allOutputs"))
            && truthy(v)?
        {
            if !p.ends_with(".drv") {
                return Err(Error::eval(format!(
                    "tried to add all-outputs context of {p}, which is not a derivation, to a string"
                )));
            }
            ctx.insert(CtxElem::DrvDeep(p.clone()));
        }
        if let Some(v) = info.get(known::outputs) {
            for o in force_list(v)?.iter() {
                ctx.insert(CtxElem::Built(p.clone(), Rc::from(string_of(o)?)));
            }
        }
    }
    Ok(Value::string(NixStr::with_ctx(s.s.clone(), ctx)))
}

// ---------------------------------------------------------------------------
// Files and the store

fn p_import(a: &[Value]) -> R<Value> {
    let (p, _) = coerce_to_path(&a[0])?;
    crate::program::import(&p)
}

fn p_scoped_import(_a: &[Value]) -> R<Value> {
    Err(Error::eval("builtins.scopedImport is not supported by compiled programs"))
}

fn p_path_exists(a: &[Value]) -> R<Value> {
    // Nix canonicalises the path lexically, so `x/..` exists when `x` does
    // not; a trailing `/` or `/.` on a string argument demands a directory.
    let must_be_dir = match force(&a[0])? {
        Value::Str(s) => s.s.ends_with(b"/") || s.s.ends_with(b"/."),
        _ => false,
    };
    let (p, _) = coerce_to_path(&a[0])?;
    let p = crate::paths::canon_path(&p);
    Ok(Value::Bool(if must_be_dir {
        std::fs::metadata(&p).is_ok_and(|m| m.is_dir())
    } else {
        std::fs::symlink_metadata(store::resolve_ancestors(&p)).is_ok()
    }))
}

fn p_read_file(a: &[Value]) -> R<Value> {
    let (p, _) = coerce_to_path(&a[0])?;
    let data = std::fs::read(&p).map_err(|e| Error::eval(format!("opening file '{p}': {e}")))?;
    let refs = store::to_store_path(&p)
        .and_then(|sp| STATE.with(|s| s.borrow().texts.get(sp).map(|t| t.1.clone())))
        .unwrap_or_default();
    let ctx = refs.into_iter().map(|r| CtxElem::Opaque(Rc::from(r))).collect();
    Ok(Value::string(NixStr::with_ctx(data, ctx)))
}

fn type_of_path(m: &std::fs::Metadata) -> &'static str {
    let t = m.file_type();
    if t.is_symlink() {
        "symlink"
    } else if t.is_dir() {
        "directory"
    } else if t.is_file() {
        "regular"
    } else {
        "unknown"
    }
}

fn p_read_file_type(a: &[Value]) -> R<Value> {
    let (p, _) = coerce_to_path(&a[0])?;
    let m = std::fs::symlink_metadata(&p).map_err(|e| Error::eval(format!("getting status of '{p}': {e}")))?;
    Ok(str_val(type_of_path(&m)))
}

fn p_read_dir(a: &[Value]) -> R<Value> {
    let (p, _) = coerce_to_path(&a[0])?;
    let rd = std::fs::read_dir(&p).map_err(|e| Error::eval(format!("reading directory '{p}': {e}")))?;
    let mut e = Vec::new();
    for ent in rd {
        let ent = ent.map_err(|e| Error::eval(format!("reading directory '{p}': {e}")))?;
        let n = ent.file_name().to_string_lossy().into_owned();
        let m = std::fs::symlink_metadata(ent.path()).map_err(|e| Error::eval(format!("{e}")))?;
        e.push(attr(intern(&n), str_val(type_of_path(&m))));
    }
    Ok(Value::attrs(Attrs::from_vec_first_wins(e)))
}

fn p_to_file(a: &[Value]) -> R<Value> {
    let n = string_of(&a[0])?;
    let s = force_str(&a[1])?;
    let mut refs = BTreeSet::new();
    for c in s.ctx_iter() {
        match c {
            CtxElem::Opaque(p) => {
                refs.insert(p.to_string());
            }
            _ => {
                return Err(Error::eval(format!(
                    "files created by builtins.toFile may not reference derivations, but {n} references {}",
                    crate::context::display_elem(c)
                )));
            }
        }
    }
    let sp = store::add_text(&n, &s.s, refs)?;
    let rc: Rc<str> = Rc::from(sp.as_str());
    Ok(Value::string(NixStr::with_ctx(
        sp.into_bytes(),
        [CtxElem::Opaque(rc)].into_iter().collect(),
    )))
}

/// Copies a source tree into the store the way `builtins.path` does.
fn add_path(path: &str, name: &str, filter: Option<Value>, method: Method, expected: Option<Hash>) -> R<Value> {
    store::check_name(name)?;
    let resolved = store::resolve_ancestors(path);
    let mut call_filter = |p: &str, t: &str| -> R<bool> {
        match &filter {
            // Nix passes the filter the path as a string, not a path value.
            Some(f) => truthy(&call_n(f, &[str_val(p), str_val(t)])?),
            None => Ok(true),
        }
    };
    let sp = match method {
        Method::Recursive => {
            let (h, _) = store::nar_hash(std::path::Path::new(&resolved), Some(&mut call_filter))?;
            if let Some(e) = &expected
                && e.algo == Algo::Sha256
                && *e != h
            {
                return Err(Error::eval(format!(
                    "hash mismatch in file '{path}':\n  specified: {}\n  got:       {}",
                    e.format(HashFormat::Sri, false),
                    h.format(HashFormat::Sri, false)
                )));
            }
            store::make_fixed_output_path(Method::Recursive, &h, name, &BTreeSet::new())
        }
        _ => {
            let data = std::fs::read(&resolved).map_err(|e| Error::eval(format!("cannot read '{path}': {e}")))?;
            let h = Hash::sha256(&data);
            store::make_fixed_output_path(Method::Flat, &h, name, &BTreeSet::new())
        }
    };
    STATE.with(|s| {
        s.borrow_mut().sources.insert(
            sp.clone(),
            store::Source {
                fs_path: resolved,
                method,
                filter,
            },
        )
    });
    let rc: Rc<str> = Rc::from(sp.as_str());
    Ok(Value::string(NixStr::with_ctx(
        sp.into_bytes(),
        [CtxElem::Opaque(rc)].into_iter().collect(),
    )))
}

fn p_path(a: &[Value]) -> R<Value> {
    let args = force_attrs(&a[0])?;
    let mut path = None;
    let mut name_v = None;
    let mut filter = None;
    let mut method = Method::Recursive;
    let mut expected = None;
    for at in args.iter() {
        match name(at.k).as_ref() {
            "path" => path = Some(coerce_to_path(&at.v)?.0),
            "name" => name_v = Some(string_of(&at.v)?),
            "filter" => filter = Some(force_function(&at.v)?),
            "recursive" => {
                method = if truthy(&at.v)? { Method::Recursive } else { Method::Flat };
            }
            "sha256" => expected = Some(Hash::parse(&string_of(&at.v)?, Some(Algo::Sha256))?),
            other => {
                return Err(Error::eval(format!(
                    "unsupported argument '{other}' to 'builtins.path'"
                )));
            }
        }
    }
    let path = path.ok_or_else(|| Error::eval("missing required 'path' attribute in the first argument to builtins.path"))?;
    let name_v = name_v.unwrap_or_else(|| crate::paths::base_name_of(&path).to_string());
    add_path(&path, &name_v, filter, method, expected)
}

fn p_filter_source(a: &[Value]) -> R<Value> {
    let filter = force_function(&a[0])?;
    let (p, _) = coerce_to_path(&a[1])?;
    let n = crate::paths::base_name_of(&p).to_string();
    add_path(&p, &n, Some(filter), Method::Recursive, None)
}

fn p_store_path(a: &[Value]) -> R<Value> {
    let (p, _) = coerce_to_path(&a[0])?;
    let sp = store::to_store_path(&p)
        .ok_or_else(|| Error::eval(format!("path '{p}' is not in the Nix store")))?;
    let rc: Rc<str> = Rc::from(sp);
    Ok(Value::string(NixStr::with_ctx(
        p.clone().into_bytes(),
        [CtxElem::Opaque(rc)].into_iter().collect(),
    )))
}

fn p_find_file(a: &[Value]) -> R<Value> {
    let sp = force_list(&a[0])?;
    let want = string_of(&a[1])?;
    for e in sp.iter() {
        let e = force_attrs(e)?;
        let prefix = match e.get(known::prefix) {
            Some(p) => string_of(p)?,
            None => String::new(),
        };
        let path = match e.get(known::path) {
            Some(p) => {
                let mut ctx = Context::new();
                String::from_utf8_lossy(&coerce_to_string(p, &mut ctx, false, false, true)?).into_owned()
            }
            None => return Err(Error::eval("attribute 'path' missing")),
        };
        let rest = if prefix.is_empty() {
            Some(want.as_str())
        } else if want == prefix {
            Some("")
        } else {
            want.strip_prefix(&format!("{prefix}/"))
        };
        if let Some(rest) = rest {
            let candidate = if rest.is_empty() { path.clone() } else { format!("{path}/{rest}") };
            if std::path::Path::new(&candidate).exists() {
                return Ok(Value::Path(Rc::from(crate::paths::canon_path(&candidate))));
            }
        }
    }
    if let Some(v) = crate::corepkgs::find(&want) {
        return Ok(v);
    }
    Err(Error::new(
        Kind::Throw,
        format!("file '{want}' was not found in the Nix search path (add it using $NIX_PATH or -I)"),
    ))
}

fn p_get_env(a: &[Value]) -> R<Value> {
    let n = string_of(&a[0])?;
    Ok(str_val(&std::env::var(&n).unwrap_or_default()))
}

fn p_placeholder(a: &[Value]) -> R<Value> {
    let o = force_str_no_ctx(&a[0])?;
    Ok(str_val(&crate::derivation::hash_placeholder(&o.lossy())))
}

fn p_derivation_strict(a: &[Value]) -> R<Value> {
    crate::derivation::derivation_strict(&a[0])
}

fn p_derivation(a: &[Value]) -> R<Value> {
    crate::derivation::derivation(&a[0])
}

fn p_to_json(a: &[Value]) -> R<Value> {
    let mut ctx = Context::new();
    let s = value_to_json(&a[0], true, &mut ctx)?;
    Ok(Value::string(NixStr::with_ctx(s.into_bytes(), ctx)))
}

fn p_from_json(a: &[Value]) -> R<Value> {
    let s = force_str(&a[0])?;
    parse_json(&s.s).context(|| "while decoding a JSON string".to_string())
}

fn p_from_toml(a: &[Value]) -> R<Value> {
    let s = force_str(&a[0])?;
    crate::toml::parse_toml(&s.lossy())
}

fn p_to_xml(a: &[Value]) -> R<Value> {
    let mut ctx = Context::new();
    let s = crate::xml::value_to_xml(&a[0], &mut ctx, true)?;
    Ok(Value::string(NixStr::with_ctx(s.into_bytes(), ctx)))
}

fn unsupported(what: &str) -> R<Value> {
    Err(Error::eval(format!(
        "builtins.{what} is not supported by compiled programs: they evaluate without network access or flakes"
    )))
}

fn p_fetchurl(_: &[Value]) -> R<Value> {
    unsupported("fetchurl")
}
fn p_fetch_tarball(_: &[Value]) -> R<Value> {
    unsupported("fetchTarball")
}
fn p_fetch_git(_: &[Value]) -> R<Value> {
    unsupported("fetchGit")
}
fn p_fetch_mercurial(_: &[Value]) -> R<Value> {
    unsupported("fetchMercurial")
}
fn p_fetch_tree(_: &[Value]) -> R<Value> {
    unsupported("fetchTree")
}
fn p_get_flake(_: &[Value]) -> R<Value> {
    unsupported("getFlake")
}
fn p_flake_ref_to_string(_: &[Value]) -> R<Value> {
    unsupported("flakeRefToString")
}
fn p_parse_flake_ref(_: &[Value]) -> R<Value> {
    unsupported("parseFlakeRef")
}

macro_rules! primops {
    ($($name:literal / $arity:literal => $f:ident,)*) => {
        /// Every built-in function, by name.
        pub static PRIMOPS: &[PrimOp] = &[$(PrimOp { name: $name, arity: $arity, f: $f },)*];
    };
}

primops! {
    "abort" / 1 => p_abort,
    "add" / 2 => p_add,
    "addDrvOutputDependencies" / 1 => p_add_drv_output_dependencies,
    "addErrorContext" / 2 => p_add_error_context,
    "all" / 2 => p_all,
    "any" / 2 => p_any,
    "appendContext" / 2 => p_append_context,
    "attrNames" / 1 => p_attr_names,
    "attrValues" / 1 => p_attr_values,
    "baseNameOf" / 1 => p_base_name_of,
    "bitAnd" / 2 => p_bit_and,
    "bitOr" / 2 => p_bit_or,
    "bitXor" / 2 => p_bit_xor,
    "break" / 1 => p_break,
    "catAttrs" / 2 => p_cat_attrs,
    "ceil" / 1 => p_ceil,
    "compareVersions" / 2 => p_compare_versions,
    "concatLists" / 1 => p_concat_lists,
    "concatMap" / 2 => p_concat_map,
    "concatStringsSep" / 2 => p_concat_strings_sep,
    "convertHash" / 1 => p_convert_hash,
    "deepSeq" / 2 => p_deep_seq,
    "derivation" / 1 => p_derivation,
    "derivationStrict" / 1 => p_derivation_strict,
    "dirOf" / 1 => p_dir_of,
    "div" / 2 => p_div,
    "elem" / 2 => p_elem,
    "elemAt" / 2 => p_elem_at,
    "fetchGit" / 1 => p_fetch_git,
    "fetchMercurial" / 1 => p_fetch_mercurial,
    "fetchTarball" / 1 => p_fetch_tarball,
    "fetchTree" / 1 => p_fetch_tree,
    "fetchurl" / 1 => p_fetchurl,
    "filter" / 2 => p_filter,
    "filterSource" / 2 => p_filter_source,
    "findFile" / 2 => p_find_file,
    "flakeRefToString" / 1 => p_flake_ref_to_string,
    "floor" / 1 => p_floor,
    "foldl'" / 3 => p_foldl,
    "fromJSON" / 1 => p_from_json,
    "fromTOML" / 1 => p_from_toml,
    "functionArgs" / 1 => p_function_args,
    "genList" / 2 => p_gen_list,
    "genericClosure" / 1 => p_generic_closure,
    "getAttr" / 2 => p_get_attr,
    "getContext" / 1 => p_get_context,
    "getEnv" / 1 => p_get_env,
    "getFlake" / 1 => p_get_flake,
    "groupBy" / 2 => p_group_by,
    "hasAttr" / 2 => p_has_attr,
    "hasContext" / 1 => p_has_context,
    "hashFile" / 2 => p_hash_file,
    "hashString" / 2 => p_hash_string,
    "head" / 1 => p_head,
    "import" / 1 => p_import,
    "intersectAttrs" / 2 => p_intersect_attrs,
    "isAttrs" / 1 => p_is_attrs,
    "isBool" / 1 => p_is_bool,
    "isFloat" / 1 => p_is_float,
    "isFunction" / 1 => p_is_function,
    "isInt" / 1 => p_is_int,
    "isList" / 1 => p_is_list,
    "isNull" / 1 => p_is_null,
    "isPath" / 1 => p_is_path,
    "isString" / 1 => p_is_string,
    "length" / 1 => p_length,
    "lessThan" / 2 => p_less_than,
    "listToAttrs" / 1 => p_list_to_attrs,
    "map" / 2 => p_map,
    "mapAttrs" / 2 => p_map_attrs,
    "match" / 2 => p_match,
    "mul" / 2 => p_mul,
    "parseDrvName" / 1 => p_parse_drv_name,
    "parseFlakeRef" / 1 => p_parse_flake_ref,
    "partition" / 2 => p_partition,
    "path" / 1 => p_path,
    "pathExists" / 1 => p_path_exists,
    "placeholder" / 1 => p_placeholder,
    "readDir" / 1 => p_read_dir,
    "readFile" / 1 => p_read_file,
    "readFileType" / 1 => p_read_file_type,
    "removeAttrs" / 2 => p_remove_attrs,
    "replaceStrings" / 3 => p_replace_strings,
    "scopedImport" / 2 => p_scoped_import,
    "seq" / 2 => p_seq,
    "sort" / 2 => p_sort,
    "split" / 2 => p_split,
    "splitVersion" / 1 => p_split_version,
    "storePath" / 1 => p_store_path,
    "stringLength" / 1 => p_string_length,
    "sub" / 2 => p_sub,
    "substring" / 3 => p_substring,
    "tail" / 1 => p_tail,
    "throw" / 1 => p_throw,
    "toFile" / 2 => p_to_file,
    "toJSON" / 1 => p_to_json,
    "toPath" / 1 => p_to_path,
    "toString" / 1 => p_to_string,
    "toXML" / 1 => p_to_xml,
    "trace" / 2 => p_trace,
    "traceVerbose" / 2 => p_trace_verbose,
    "tryEval" / 1 => p_try_eval,
    "typeOf" / 1 => p_type_of,
    "unsafeDiscardOutputDependency" / 1 => p_unsafe_discard_output_dependency,
    "unsafeDiscardStringContext" / 1 => p_unsafe_discard_string_context,
    "unsafeGetAttrPos" / 2 => p_unsafe_get_attr_pos,
    "warn" / 2 => p_warn,
    "zipAttrsWith" / 2 => p_zip_attrs_with,
}

fn p_to_path(a: &[Value]) -> R<Value> {
    let (p, ctx) = coerce_to_path(&a[0])?;
    Ok(Value::string(NixStr::with_ctx(crate::paths::canon_path(&p).into_bytes(), ctx)))
}

/// The names bound at the top level without the `__` prefix. Every other
/// built-in is bound at the top level as `__<name>`.
pub const BARE: &[&str] = &[
    "abort",
    "baseNameOf",
    "break",
    "builtins",
    "derivation",
    "derivationStrict",
    "dirOf",
    "false",
    "fetchGit",
    "fetchMercurial",
    "fetchTarball",
    "fetchTree",
    "fromTOML",
    "import",
    "isNull",
    "map",
    "null",
    "placeholder",
    "removeAttrs",
    "scopedImport",
    "throw",
    "toString",
    "true",
];

/// The system the program evaluates for by default (`builtins.currentSystem`).
pub fn current_system() -> String {
    if let Ok(s) = std::env::var("NIXPKGS_RUST_SYSTEM") {
        return s;
    }
    let arch = std::env::consts::ARCH;
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        o => o,
    };
    format!("{arch}-{os}")
}

/// The entries of `builtins.nixPath`: `NIX_PATH` and the `-I` options, as
/// `{ path, prefix }` sets.
pub fn nix_path_entries() -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Relative entries are relative to the working directory, as in Nix.
    let absolute = |p: &str| -> String {
        if p.starts_with('/') || p.contains("://") {
            p.to_string()
        } else {
            std::path::absolute(p)
                .map(|a| crate::paths::canon_path(&a.to_string_lossy()))
                .unwrap_or_else(|_| p.to_string())
        }
    };
    let mut add = |s: &str| {
        for e in s.split(':').filter(|e| !e.is_empty()) {
            match e.split_once('=') {
                Some((prefix, path)) => out.push((prefix.to_string(), absolute(path))),
                None => out.push((String::new(), absolute(e))),
            }
        }
    };
    if let Ok(extra) = std::env::var("NIXPKGS_RUST_INCLUDE") {
        add(&extra);
    }
    if let Ok(p) = std::env::var("NIX_PATH") {
        add(&p);
    }
    out
}

fn constants() -> Vec<(&'static str, Value)> {
    let nix_path = Value::list(
        nix_path_entries()
            .into_iter()
            .map(|(prefix, path)| {
                Value::attrs(Attrs::from_vec_first_wins(vec![
                    attr(known::path, str_val(&path)),
                    attr(known::prefix, str_val(&prefix)),
                ]))
            })
            .collect(),
    );
    vec![
        ("true", Value::Bool(true)),
        ("false", Value::Bool(false)),
        ("null", Value::Null),
        ("currentSystem", str_val(&current_system())),
        (
            "currentTime",
            Value::Int(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            ),
        ),
        ("langVersion", Value::Int(6)),
        ("nixVersion", str_val("2.34.9")),
        ("storeDir", str_val(store::store_dir())),
        ("nixPath", nix_path),
    ]
}

struct Env {
    builtins: Value,
    globals: Rc<Attrs>,
}

thread_local! {
    static ENV: OnceCell<Env> = const { OnceCell::new() };
}

fn make_env() -> Env {
    let builtins = Value::blank();
    let mut b = Vec::new();
    let mut g = Vec::new();
    for op in PRIMOPS {
        let v = Value::prim(op);
        let k = intern(op.name);
        b.push(attr(k, v.clone()));
        if BARE.contains(&op.name) {
            g.push(attr(k, v));
        } else {
            g.push(attr(intern(&format!("__{}", op.name)), v));
        }
    }
    for (n, v) in constants() {
        let k = intern(n);
        b.push(attr(k, v.clone()));
        if BARE.contains(&n) {
            g.push(attr(k, v));
        } else {
            g.push(attr(intern(&format!("__{n}")), v));
        }
    }
    b.push(attr(known::builtins, builtins.clone()));
    g.push(attr(known::builtins, builtins.clone()));
    builtins.init_value(Value::attrs(Attrs::from_vec_first_wins(b)));
    Env {
        builtins,
        globals: Rc::new(Attrs::from_vec_first_wins(g)),
    }
}

/// The `builtins` set.
pub fn builtins() -> Value {
    ENV.with(|e| e.get_or_init(make_env).builtins.clone())
}

/// A name bound at the top level (`map`, `__attrNames`, `true`, ...).
pub fn global(k: Sym) -> R<Value> {
    ENV.with(|e| {
        e.get_or_init(make_env)
            .globals
            .get(k)
            .cloned()
            .ok_or_else(|| Error::eval(format!("undefined variable '{}'", name(k))))
    })
}

/// Whether a name is bound at the top level.
pub fn is_global(n: &str) -> bool {
    if BARE.contains(&n) {
        return true;
    }
    match n.strip_prefix("__") {
        Some(rest) => {
            !BARE.contains(&rest)
                && (PRIMOPS.iter().any(|p| p.name == rest)
                    || matches!(rest, "currentSystem" | "currentTime" | "langVersion" | "nixVersion" | "storeDir" | "nixPath"))
        }
        None => false,
    }
}

/// Renders the strict value of an expression, for the language tests.
pub fn print(v: &Value) -> R<String> {
    print_strict(v)
}
