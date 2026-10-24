//! `derivation`, `derivationStrict`, and the store derivation format.
//!
//! The paths computed here must equal those Nix computes, so the ATerm
//! serialisation, the "modulo" hashing of input derivations and the rules for
//! turning string context into inputs follow `libstore/derivations.cc` and
//! `prim_derivationStrict` in Nix 2.34.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::attrs::{Attr, Attrs};
use crate::error::{Context as _, Error, R};
use crate::json::value_to_json;
use crate::ops::{coerce_to_string, force_attrs, force_list, force_str, force_str_no_ctx, truthy};
use crate::store::{
    Algo, Hash, Method, STATE, check_name, make_fixed_output_path, make_output_path, make_text_path,
};
use crate::string::{Context, CtxElem, NixStr};
use crate::symbol::{intern, known, name};
use crate::value::{Pos, Value, force};

/// One output of a derivation.
#[derive(Clone, Debug)]
pub enum Output {
    /// The path is computed from the derivation.
    InputAddressed(String),
    /// The path is computed from the declared hash of the output.
    Fixed { method: Method, hash: Hash },
    /// Content-addressed with a hash known only after building.
    Floating { method: Method, algo: Algo },
    /// Input-addressed, with the path not yet known.
    Deferred,
    /// An impure derivation output.
    Impure { method: Method, algo: Algo },
}

/// A store derivation.
#[derive(Clone, Debug, Default)]
pub struct Derivation {
    pub name: String,
    pub outputs: BTreeMap<String, Output>,
    /// Input derivations and the outputs used from each.
    pub input_drvs: BTreeMap<String, BTreeSet<String>>,
    pub input_srcs: BTreeSet<String>,
    pub platform: String,
    pub builder: Vec<u8>,
    pub args: Vec<Vec<u8>>,
    pub env: BTreeMap<String, Vec<u8>>,
    /// The JSON of structured attributes, written to the `__json` variable.
    pub structured: Option<String>,
}

fn method_algo(method: Method, algo: Algo) -> String {
    format!("{}{}", method.prefix(), algo.name())
}

impl Derivation {
    /// The path of a fixed output.
    pub fn fixed_path(&self, method: Method, hash: &Hash, output: &str) -> String {
        let n = if output == "out" {
            self.name.clone()
        } else {
            format!("{}-{output}", self.name)
        };
        make_fixed_output_path(method, hash, &n, &BTreeSet::new())
    }

    /// The output path of each output that has one.
    pub fn output_paths(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for (o, out) in &self.outputs {
            match out {
                Output::InputAddressed(p) => {
                    m.insert(o.clone(), p.clone());
                }
                Output::Fixed { method, hash } => {
                    m.insert(o.clone(), self.fixed_path(*method, hash, o));
                }
                _ => {}
            }
        }
        m
    }

    /// The ATerm form of the derivation. With `actual_inputs`, the input
    /// derivations are replaced by the given map, as modulo hashing does.
    pub fn unparse(&self, mask_outputs: bool, actual_inputs: Option<&BTreeMap<String, BTreeSet<String>>>) -> Vec<u8> {
        let mut s: Vec<u8> = Vec::with_capacity(4096);
        s.extend_from_slice(b"Derive([");
        for (n, (o, out)) in self.outputs.iter().enumerate() {
            if n > 0 {
                s.push(b',');
            }
            s.push(b'(');
            unquoted(&mut s, o.as_bytes());
            let (path, algo, hash) = match out {
                Output::InputAddressed(p) => (if mask_outputs { String::new() } else { p.clone() }, String::new(), String::new()),
                Output::Fixed { method, hash } => (
                    if mask_outputs { String::new() } else { self.fixed_path(*method, hash, o) },
                    method_algo(*method, hash.algo),
                    hash.hex(),
                ),
                Output::Floating { method, algo } => (String::new(), method_algo(*method, *algo), String::new()),
                Output::Deferred => (String::new(), String::new(), String::new()),
                Output::Impure { method, algo } => (String::new(), method_algo(*method, *algo), "impure".to_string()),
            };
            s.push(b',');
            unquoted(&mut s, path.as_bytes());
            s.push(b',');
            unquoted(&mut s, algo.as_bytes());
            s.push(b',');
            unquoted(&mut s, hash.as_bytes());
            s.push(b')');
        }
        s.extend_from_slice(b"],[");
        let inputs = actual_inputs.unwrap_or(&self.input_drvs);
        for (n, (d, outs)) in inputs.iter().enumerate() {
            if n > 0 {
                s.push(b',');
            }
            s.push(b'(');
            unquoted(&mut s, d.as_bytes());
            s.extend_from_slice(b",[");
            for (k, o) in outs.iter().enumerate() {
                if k > 0 {
                    s.push(b',');
                }
                unquoted(&mut s, o.as_bytes());
            }
            s.extend_from_slice(b"])");
        }
        s.extend_from_slice(b"],[");
        for (n, p) in self.input_srcs.iter().enumerate() {
            if n > 0 {
                s.push(b',');
            }
            unquoted(&mut s, p.as_bytes());
        }
        s.extend_from_slice(b"],");
        unquoted(&mut s, self.platform.as_bytes());
        s.push(b',');
        escaped(&mut s, &self.builder);
        s.extend_from_slice(b",[");
        for (n, a) in self.args.iter().enumerate() {
            if n > 0 {
                s.push(b',');
            }
            escaped(&mut s, a);
        }
        s.extend_from_slice(b"],[");
        let json;
        let mut env: Vec<(&str, &[u8])> = self.env.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
        if let Some(j) = &self.structured {
            json = j.clone();
            env.push(("__json", json.as_bytes()));
            env.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        }
        for (n, (k, v)) in env.iter().enumerate() {
            if n > 0 {
                s.push(b',');
            }
            s.push(b'(');
            escaped(&mut s, k.as_bytes());
            s.push(b',');
            let masked = mask_outputs && self.outputs.contains_key(*k);
            escaped(&mut s, if masked { b"" } else { v });
            s.push(b')');
        }
        s.extend_from_slice(b"])");
        s
    }

    /// The references of the `.drv` file: its sources and input derivations.
    pub fn references(&self) -> BTreeSet<String> {
        let mut r = self.input_srcs.clone();
        r.extend(self.input_drvs.keys().cloned());
        r
    }

    fn is_fixed(&self) -> bool {
        self.outputs.values().any(|o| matches!(o, Output::Fixed { .. }))
    }
}

fn unquoted(s: &mut Vec<u8>, v: &[u8]) {
    s.push(b'"');
    s.extend_from_slice(v);
    s.push(b'"');
}

fn escaped(s: &mut Vec<u8>, v: &[u8]) {
    s.push(b'"');
    for &c in v {
        match c {
            b'"' | b'\\' => {
                s.push(b'\\');
                s.push(c);
            }
            b'\n' => s.extend_from_slice(b"\\n"),
            b'\r' => s.extend_from_slice(b"\\r"),
            b'\t' => s.extend_from_slice(b"\\t"),
            _ => s.push(c),
        }
    }
    s.push(b'"');
}

/// The modulo hash of each output of a derivation.
pub type DrvHash = BTreeMap<String, Hash>;

/// `hashDerivationModulo`.
pub fn hash_derivation_modulo(drv: &Derivation, mask_outputs: bool) -> R<DrvHash> {
    if drv.is_fixed() {
        let mut m = BTreeMap::new();
        for (o, out) in &drv.outputs {
            if let Output::Fixed { method, hash } = out {
                let s = format!(
                    "fixed:out:{}:{}:{}",
                    method_algo(*method, hash.algo),
                    hash.hex(),
                    drv.fixed_path(*method, hash, o)
                );
                m.insert(o.clone(), Hash::sha256(s.as_bytes()));
            }
        }
        return Ok(m);
    }
    let mut inputs2: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (d, outs) in &drv.input_drvs {
        let h = path_derivation_modulo(d)?;
        for o in outs {
            let hh = h
                .get(o)
                .ok_or_else(|| Error::eval(format!("no hash for output '{o}' of derivation '{}'", drv.name)))?;
            inputs2.entry(hh.hex()).or_default().insert(o.clone());
        }
    }
    let hash = Hash::sha256(&drv.unparse(mask_outputs, Some(&inputs2)));
    Ok(drv.outputs.keys().map(|o| (o.clone(), hash.clone())).collect())
}

fn path_derivation_modulo(drv_path: &str) -> R<DrvHash> {
    if let Some(h) = STATE.with(|s| s.borrow().drv_hashes.get(drv_path).cloned()) {
        return Ok(h);
    }
    let drv = lookup_drv(drv_path)?;
    let h = hash_derivation_modulo(&drv, false)?;
    STATE.with(|s| s.borrow_mut().drv_hashes.insert(drv_path.to_string(), h.clone()));
    Ok(h)
}

/// A derivation this evaluation created, or one read from the store.
pub fn lookup_drv(drv_path: &str) -> R<Rc<Derivation>> {
    if let Some(d) = STATE.with(|s| s.borrow().drvs.get(drv_path).cloned()) {
        return Ok(d);
    }
    let text = std::fs::read(drv_path)
        .map_err(|e| Error::eval(format!("cannot read derivation '{drv_path}': {e}")))?;
    let d = Rc::new(crate::aterm::parse(&text, drv_path)?);
    STATE.with(|s| s.borrow_mut().drvs.insert(drv_path.to_string(), d.clone()));
    Ok(d)
}

/// The closure of a `.drv` file in the store graph: itself, the derivations
/// it depends on, and their sources.
fn drv_closure(drv_path: &str, out: &mut BTreeSet<String>) -> R<()> {
    if !out.insert(drv_path.to_string()) {
        return Ok(());
    }
    let d = lookup_drv(drv_path)?;
    for s in &d.input_srcs {
        source_closure(s, out);
    }
    for i in d.input_drvs.keys() {
        drv_closure(i, out)?;
    }
    Ok(())
}

fn source_closure(p: &str, out: &mut BTreeSet<String>) {
    if !out.insert(p.to_string()) {
        return;
    }
    let refs = STATE.with(|s| s.borrow().texts.get(p).map(|t| t.1.clone()));
    for r in refs.into_iter().flatten() {
        if r.ends_with(".drv") {
            let _ = drv_closure(&r, out);
        } else {
            source_closure(&r, out);
        }
    }
}

/// The placeholder `builtins.placeholder` returns for an output.
pub fn hash_placeholder(output: &str) -> String {
    let h = Hash::sha256(format!("nix-output:{output}").as_bytes());
    format!("/{}", crate::store::nix32_encode(&h.bytes))
}

fn parse_method(s: &str) -> R<Method> {
    Ok(match s {
        "recursive" | "nar" => Method::Recursive,
        "flat" => Method::Flat,
        "text" => {
            return Err(Error::eval(
                "experimental Nix feature 'dynamic-derivations' is disabled; add '--extra-experimental-features dynamic-derivations' to enable it",
            ));
        }
        "git" => {
            return Err(Error::eval(
                "experimental Nix feature 'git-hashing' is disabled; add '--extra-experimental-features git-hashing' to enable it",
            ));
        }
        _ => return Err(Error::eval(format!("invalid value '{s}' for 'outputHashMode' attribute"))),
    })
}

/// `builtins.derivationStrict`.
pub fn derivation_strict(arg: &Value) -> R<Value> {
    let attrs = force_attrs(arg)?;
    let name_v = attrs
        .get(known::name)
        .ok_or_else(|| Error::eval("attribute 'name' missing"))?;
    let drv_name = force_str_no_ctx(name_v)
        .context(|| "while evaluating the derivation attribute 'name'".to_string())?
        .lossy()
        .into_owned();
    derivation_strict_inner(&attrs, &drv_name).context(|| format!("while evaluating derivation '{drv_name}'"))
}

fn derivation_strict_inner(attrs: &Attrs, drv_name: &str) -> R<Value> {
    check_name(drv_name).map_err(|e| {
        Error::eval(format!(
            "invalid derivation name: {}. Please pass a different 'name'.",
            e.msg
        ))
    })?;
    let structured = match attrs.get(known::structuredAttrs) {
        Some(v) => truthy(v)?,
        None => false,
    };
    let ignore_nulls = match attrs.get(known::ignoreNulls) {
        Some(v) => truthy(v)?,
        None => false,
    };

    let mut drv = Derivation {
        name: drv_name.to_string(),
        ..Default::default()
    };
    let mut ctx = Context::new();
    let mut json = BTreeMap::<String, String>::new();
    let mut output_hash: Option<String> = None;
    let mut output_hash_algo: Option<Algo> = None;
    let mut method: Option<Method> = None;
    let mut outputs: Vec<String> = vec!["out".to_string()];

    let set_outputs = |list: Vec<String>, outputs: &mut Vec<String>| -> R<()> {
        let mut seen = BTreeSet::new();
        for o in &list {
            if !seen.insert(o.clone()) {
                return Err(Error::eval(format!("duplicate derivation output '{o}'")));
            }
            if o == "drvPath" {
                return Err(Error::eval("invalid derivation output name 'drvPath'"));
            }
        }
        if list.is_empty() {
            return Err(Error::eval("derivation cannot have an empty set of outputs"));
        }
        *outputs = list;
        Ok(())
    };

    for at in attrs.by_name() {
        if at.k == known::ignoreNulls {
            continue;
        }
        let key = name(at.k);
        let res: R<()> = (|| {
            if ignore_nulls && matches!(force(&at.v)?, Value::Null) {
                return Ok(());
            }
            if at.k == known::contentAddressed {
                if truthy(&at.v)? {
                    return Err(Error::eval(
                        "experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it",
                    ));
                }
                return Ok(());
            }
            if at.k == known::impure {
                if truthy(&at.v)? {
                    return Err(Error::eval(
                        "experimental Nix feature 'impure-derivations' is disabled; add '--extra-experimental-features impure-derivations' to enable it",
                    ));
                }
                return Ok(());
            }
            if at.k == known::args {
                for e in force_list(&at.v)?.iter() {
                    drv.args.push(
                        coerce_to_string(e, &mut ctx, true, true, true)
                            .context(|| "while evaluating an element of the argument list".to_string())?,
                    );
                }
                return Ok(());
            }
            if structured {
                if at.k == known::structuredAttrs {
                    return Ok(());
                }
                json.insert(key.to_string(), value_to_json(&at.v, true, &mut ctx)?);
                if at.k == known::builder {
                    let s = force_str(&at.v)?;
                    ctx.extend(s.ctx_iter().cloned());
                    drv.builder = s.s.to_vec();
                } else if at.k == known::system {
                    drv.platform = force_str_no_ctx(&at.v)?.lossy().into_owned();
                } else if at.k == known::outputHash {
                    output_hash = Some(force_str_no_ctx(&at.v)?.lossy().into_owned());
                } else if at.k == known::outputHashAlgo {
                    output_hash_algo = parse_algo_opt(&force_str_no_ctx(&at.v)?.lossy())?;
                } else if at.k == known::outputHashMode {
                    method = Some(parse_method(&force_str_no_ctx(&at.v)?.lossy())?);
                } else if at.k == known::outputs {
                    let mut list = Vec::new();
                    for e in force_list(&at.v)?.iter() {
                        list.push(force_str_no_ctx(e)?.lossy().into_owned());
                    }
                    set_outputs(list, &mut outputs)?;
                }
                return Ok(());
            }
            let s = coerce_to_string(&at.v, &mut ctx, true, true, true)?;
            if at.k == known::json {
                drv.structured = Some(String::from_utf8_lossy(&s).into_owned());
                return Ok(());
            }
            let st = String::from_utf8_lossy(&s).into_owned();
            if at.k == known::builder {
                drv.builder = s.clone();
            } else if at.k == known::system {
                drv.platform = st.clone();
            } else if at.k == known::outputHash {
                output_hash = Some(st.clone());
            } else if at.k == known::outputHashAlgo {
                output_hash_algo = parse_algo_opt(&st)?;
            } else if at.k == known::outputHashMode {
                method = Some(parse_method(&st)?);
            } else if at.k == known::outputs {
                set_outputs(st.split([' ', '\t', '\n', '\r']).filter(|x| !x.is_empty()).map(String::from).collect(), &mut outputs)?;
            }
            drv.env.insert(key.to_string(), s);
            Ok(())
        })();
        res.context(|| format!("while evaluating attribute '{key}' of derivation '{drv_name}'"))?;
    }

    if structured {
        drv.structured = Some(crate::json::object_to_string(&json));
    }

    for c in &ctx {
        match c {
            CtxElem::DrvDeep(d) => {
                let mut refs = BTreeSet::new();
                drv_closure(d, &mut refs)?;
                for j in refs {
                    if j.ends_with(".drv") {
                        let outs = lookup_drv(&j)?.outputs.keys().cloned().collect();
                        drv.input_drvs.insert(j.clone(), outs);
                    }
                    drv.input_srcs.insert(j);
                }
            }
            CtxElem::Built(d, o) => {
                drv.input_drvs.entry(d.to_string()).or_default().insert(o.to_string());
            }
            CtxElem::Opaque(p) => {
                drv.input_srcs.insert(p.to_string());
            }
        }
    }

    if drv.builder.is_empty() {
        return Err(Error::eval("required attribute 'builder' missing"));
    }
    if drv.platform.is_empty() {
        return Err(Error::eval("required attribute 'system' missing"));
    }
    if drv_name.ends_with(".drv") {
        return Err(Error::eval(
            "derivation names are allowed to end in '.drv' only if they produce a single derivation file",
        ));
    }

    if let Some(oh) = output_hash {
        if outputs.len() != 1 || outputs[0] != "out" {
            return Err(Error::eval("multiple outputs are not supported in fixed-output derivations"));
        }
        let hash = if oh.is_empty() {
            let algo = output_hash_algo.unwrap_or(Algo::Sha256);
            eprintln!(
                "warning: found empty hash, assuming '{}'",
                Hash { algo, bytes: vec![0; algo.size()] }.format(crate::store::HashFormat::Sri, false)
            );
            Hash { algo, bytes: vec![0; algo.size()] }
        } else {
            Hash::parse(&oh, output_hash_algo)?
        };
        let method = method.unwrap_or(Method::Flat);
        let path = drv.fixed_path(method, &hash, "out");
        drv.env.insert("out".to_string(), path.into_bytes());
        drv.outputs.insert("out".to_string(), Output::Fixed { method, hash });
    } else {
        for o in &outputs {
            drv.env.insert(o.clone(), Vec::new());
            drv.outputs.insert(o.clone(), Output::Deferred);
        }
        let hashes = hash_derivation_modulo(&drv, true)?;
        for o in &outputs {
            let p = make_output_path(o, &hashes[o], drv_name);
            drv.env.insert(o.clone(), p.clone().into_bytes());
            drv.outputs.insert(o.clone(), Output::InputAddressed(p));
        }
    }

    let text = drv.unparse(false, None);
    let drv_path = make_text_path(&format!("{drv_name}.drv"), &text, &drv.references());
    let h = hash_derivation_modulo(&drv, false)?;
    let paths = drv.output_paths();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.drv_hashes.insert(drv_path.clone(), h);
        s.drvs.insert(drv_path.clone(), Rc::new(drv));
    });

    let drv_path_rc: Rc<str> = Rc::from(drv_path.as_str());
    let mut out = vec![Attr {
        k: known::drvPath,
        v: Value::string(NixStr::with_ctx(
            drv_path.clone().into_bytes(),
            [CtxElem::DrvDeep(drv_path_rc.clone())].into_iter().collect(),
        )),
        pos: Pos::default(),
    }];
    for (o, p) in paths {
        out.push(Attr {
            k: intern(&o),
            v: Value::string(NixStr::with_ctx(
                p.into_bytes(),
                [CtxElem::Built(drv_path_rc.clone(), Rc::from(o.as_str()))].into_iter().collect(),
            )),
            pos: Pos::default(),
        });
    }
    Ok(Value::attrs(Attrs::from_vec_first_wins(out)))
}

fn parse_algo_opt(s: &str) -> R<Option<Algo>> {
    if s.is_empty() {
        return Ok(None);
    }
    Algo::parse(s)
        .map(Some)
        .ok_or_else(|| Error::eval(format!("unknown hash algorithm '{s}'")))
}

/// `derivation`: the lazy wrapper around `derivationStrict` that Nix defines
/// in its built-in `derivation.nix`. The argument is forced to a set; every
/// other step stays lazy until its result is needed.
pub fn derivation(arg: &Value) -> R<Value> {
    let drv_attrs = force_attrs(arg)?;
    let outputs_v = drv_attrs
        .get(known::outputs)
        .cloned()
        .unwrap_or_else(|| Value::list(vec![Value::str(&b"out"[..])]));
    let strict = {
        let a = Value::Attrs(drv_attrs.clone());
        Value::native(move || derivation_strict(&a))
    };
    let common = Value::blank();
    let outputs_list = {
        let common = common.clone();
        let strict = strict.clone();
        Value::native(move || {
            let outs = force_list(&outputs_v)?;
            let mut v = Vec::with_capacity(outs.len());
            for o in outs.iter() {
                v.push(output_element(o.clone(), common.clone(), strict.clone()));
            }
            Ok(Value::list(v))
        })
    };
    {
        let outputs_list = outputs_list.clone();
        let drv_attrs = drv_attrs.clone();
        common.init_value(Value::native(move || {
            let mut by_output = Vec::new();
            let list = force_list(&outputs_list)?;
            for e in list.iter() {
                let e = force_attrs(e)?;
                let n = force_str(e.get(known::name).unwrap())?;
                by_output.push(Attr {
                    k: intern(&n.lossy()),
                    v: e.get(known::value).unwrap().clone(),
                    pos: Pos::default(),
                });
            }
            let mut a = Attrs::update(&drv_attrs, &Attrs::from_vec_first_wins(by_output));
            let all: Vec<Value> = list.iter().map(|e| Value::sel(e.clone(), known::value)).collect();
            a = Attrs::update(
                &a,
                &Attrs::from_vec_first_wins(vec![
                    Attr {
                        k: known::all,
                        v: Value::list(all),
                        pos: Pos::default(),
                    },
                    Attr {
                        k: known::drvAttrs,
                        v: Value::Attrs(drv_attrs.clone()),
                        pos: Pos::default(),
                    },
                ]),
            );
            Ok(Value::attrs(a))
        }));
    }
    let head = Value::native(move || {
        let list = force_list(&outputs_list)?;
        match list.first() {
            Some(e) => crate::ops::select(e, known::value),
            None => Err(Error::eval("'builtins.head' called on an empty list")),
        }
    });
    Ok(head)
}

fn output_element(output_name: Value, common: Value, strict: Value) -> Value {
    let value = {
        let output_name = output_name.clone();
        Value::native(move || {
            let n = force_str(&output_name)?;
            let k = intern(&n.lossy());
            let extra = Attrs::from_vec_first_wins(vec![
                Attr {
                    k: known::outPath,
                    v: Value::sel(strict.clone(), k),
                    pos: Pos::default(),
                },
                Attr {
                    k: known::drvPath,
                    v: Value::sel(strict.clone(), known::drvPath),
                    pos: Pos::default(),
                },
                Attr {
                    k: known::type_,
                    v: Value::str(&b"derivation"[..]),
                    pos: Pos::default(),
                },
                Attr {
                    k: known::outputName,
                    v: output_name.clone(),
                    pos: Pos::default(),
                },
            ]);
            let c = force_attrs(&common)?;
            Ok(Value::attrs(Attrs::update(&c, &extra)))
        })
    };
    Value::attrs(Attrs::from_vec_first_wins(vec![
        Attr {
            k: known::name,
            v: output_name,
            pos: Pos::default(),
        },
        Attr {
            k: known::value,
            v: value,
            pos: Pos::default(),
        },
    ]))
}
