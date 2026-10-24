//! Building a derivation through the Nix daemon.
//!
//! Evaluation computed the paths of the derivations, sources and texts it
//! created without writing anything. Building adds those the store does not
//! have yet, in dependency order, checking that the daemon assigns each the
//! path evaluation computed, and then asks the daemon to build the requested
//! output. The daemon builds or substitutes every dependency first.

use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;

use crate::derivation::{Derivation, lookup_drv};
use crate::error::{Error, R};
use crate::ops::{force_attrs, force_str, is_derivation, truthy};
use crate::store::{Method, STATE};
use crate::symbol::known;
use crate::value::{Value, call_n};

/// How to build.
#[derive(Default)]
pub struct BuildOpts {
    /// Where to create the `result` symbolic link, or `None` for no link.
    pub out_link: Option<String>,
    /// Whether to show the daemon's log.
    pub verbose: bool,
}

fn daemon_err(e: String) -> Box<Error> {
    Error::eval(e)
}

/// The store object name of a store path: what follows the hash.
fn name_of(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.get(33..).unwrap_or(base)
}

/// Everything a derivation needs in the store, in an order in which each
/// object comes after the objects it refers to.
#[derive(Default)]
struct Closure {
    drvs: Vec<(String, Rc<Derivation>)>,
    texts: Vec<String>,
    sources: Vec<String>,
    /// Paths neither this evaluation created nor that are derivations: they
    /// must already be in the store.
    existing: BTreeSet<String>,
    seen: HashSet<String>,
}

impl Closure {
    fn add_path(&mut self, p: &str) -> R<()> {
        if !self.seen.insert(p.to_string()) {
            return Ok(());
        }
        if p.ends_with(".drv") {
            let d = lookup_drv(p)?;
            for i in d.input_drvs.keys() {
                self.add_path(i)?;
            }
            for s in &d.input_srcs {
                self.add_path(s)?;
            }
            self.drvs.push((p.to_string(), d));
            return Ok(());
        }
        let text_refs = STATE.with(|s| s.borrow().texts.get(p).map(|t| t.1.clone()));
        if let Some(refs) = text_refs {
            for r in &refs {
                self.add_path(r)?;
            }
            self.texts.push(p.to_string());
        } else if STATE.with(|s| s.borrow().sources.contains_key(p)) {
            self.sources.push(p.to_string());
        } else {
            self.existing.insert(p.to_string());
        }
        Ok(())
    }

    fn all(&self) -> BTreeSet<String> {
        let mut s: BTreeSet<String> = self.drvs.iter().map(|d| d.0.clone()).collect();
        s.extend(self.texts.iter().cloned());
        s.extend(self.sources.iter().cloned());
        s.extend(self.existing.iter().cloned());
        s
    }
}

fn check_path(what: &str, expected: &str, got: &str) -> R<()> {
    if expected != got {
        return Err(Error::eval(format!(
            "the Nix daemon stored {what} at '{got}', but evaluation computed '{expected}'"
        )));
    }
    Ok(())
}

/// Adds a source tree to the store.
fn upload_source(d: &mut crate::daemon::Daemon, path: &str) -> R<()> {
    let (fs_path, method, filter) = STATE.with(|s| {
        let s = s.borrow();
        let src = &s.sources[path];
        (src.fs_path.clone(), src.method, src.filter.clone())
    });
    let method_str = match method {
        Method::Recursive => "fixed:r:sha256",
        Method::Flat => "fixed:sha256",
        Method::Text => "text:sha256",
    };
    let mut write = |sink: &mut dyn crate::store::NarSink| -> Result<(), String> {
        match method {
            Method::Flat => {
                let data = std::fs::read(&fs_path).map_err(|e| format!("cannot read '{fs_path}': {e}"))?;
                sink.write(&data);
                Ok(())
            }
            _ => {
                let mut f = |p: &str, t: &str| -> R<bool> {
                    match &filter {
                        // As when evaluation hashed the source: the path as a string.
                        Some(f) => truthy(&call_n(f, &[Value::str(p.as_bytes()), Value::str(t.as_bytes())])?),
                        None => Ok(true),
                    }
                };
                crate::store::dump_path(std::path::Path::new(&fs_path), sink, Some(&mut f)).map_err(|e| e.to_string())
            }
        }
    };
    let got = d
        .add_to_store(name_of(path), method_str, &BTreeSet::new(), &mut write)
        .map_err(daemon_err)?;
    check_path(&format!("source '{fs_path}'"), path, &got)
}

/// Builds the selected output of a derivation value and returns its path.
pub fn build(v: &Value, opts: &BuildOpts) -> R<String> {
    let a = force_attrs(v)?;
    if !is_derivation(&a)? {
        return Err(Error::eval("the value to build is not a derivation"));
    }
    let drv_path = force_str(a.get(known::drvPath).ok_or_else(|| Error::eval("the derivation has no 'drvPath'"))?)?
        .lossy()
        .into_owned();
    let output = match a.get(known::outputName) {
        Some(o) => force_str(o)?.lossy().into_owned(),
        None => "out".to_string(),
    };

    let mut c = Closure::default();
    c.add_path(&drv_path)?;
    let mut d = crate::daemon::Daemon::connect().map_err(daemon_err)?;
    d.verbose = opts.verbose;
    let valid = d.query_valid_paths(&c.all()).map_err(daemon_err)?;
    let missing: Vec<&String> = c.existing.iter().filter(|p| !valid.contains(*p)).collect();
    if let Some(p) = missing.first() {
        return Err(Error::eval(format!(
            "'{p}' is needed but neither in the store nor created by this evaluation"
        )));
    }
    let mut added = 0;
    for s in &c.sources {
        if !valid.contains(s) {
            upload_source(&mut d, s)?;
            added += 1;
        }
    }
    for t in &c.texts {
        if !valid.contains(t) {
            let (contents, refs) = STATE.with(|s| s.borrow().texts[t].clone());
            let got = d.add_text(name_of(t), &contents, &refs).map_err(daemon_err)?;
            check_path("a text", t, &got)?;
            added += 1;
        }
    }
    for (p, drv) in &c.drvs {
        if !valid.contains(p) {
            let got = d
                .add_text(name_of(p), &drv.unparse(false, None), &drv.references())
                .map_err(daemon_err)?;
            check_path("a derivation", p, &got)?;
            added += 1;
        }
    }
    if opts.verbose {
        eprintln!(
            "{} derivations, {} sources and {} texts in the closure; {added} added to the store",
            c.drvs.len(),
            c.sources.len(),
            c.texts.len()
        );
    }
    d.build_paths(&[format!("{drv_path}!{output}")]).map_err(daemon_err)?;
    let top = lookup_drv(&drv_path)?;
    let out_path = top
        .output_paths()
        .get(&output)
        .cloned()
        .ok_or_else(|| Error::eval(format!("the derivation has no output '{output}' with a known path")))?;
    if let Some(link) = &opts.out_link {
        let link = if output == "out" { link.clone() } else { format!("{link}-{output}") };
        let abs = std::path::absolute(&link).map_err(|e| Error::eval(format!("{e}")))?;
        let _ = std::fs::remove_file(&abs);
        std::os::unix::fs::symlink(&out_path, &abs)
            .map_err(|e| Error::eval(format!("cannot create '{}': {e}", abs.display())))?;
        d.add_indirect_root(&abs.to_string_lossy()).map_err(daemon_err)?;
    }
    Ok(out_path)
}
