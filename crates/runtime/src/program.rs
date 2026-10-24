//! A compiled program: the Nix files the compiler turned into Rust, and the
//! machinery that lets them import each other.
//!
//! The compiler compiles every `.nix` file under one root directory. At run
//! time the root is wherever that tree lives (for nixpkgs, its store path);
//! `import` of a file below the root runs the compiled code of that file, so
//! no Nix source is parsed when the program runs. Files are still read from
//! the root for everything else: `readFile`, `readDir`, and copying sources
//! into the store.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::error::{Context as _, Error, R};
use crate::paths::canon_path;
use crate::value::{CodeId, CrateTables, Value, force, run};

/// One compiled file.
pub struct FileEntry {
    /// The path relative to the root, without a leading slash.
    pub path: &'static str,
    /// The function that evaluates the file. `None` when the file failed to
    /// compile; importing it reports `error`.
    pub code: Option<CodeId>,
    /// Why the file did not compile.
    pub error: &'static str,
}

/// Everything the compiler generated, as the program hands it to [`install`].
pub struct Program {
    pub symbols: &'static [&'static str],
    pub crates: &'static [&'static CrateTables],
    pub files: &'static [FileEntry],
}

struct State {
    root: String,
    files: &'static [FileEntry],
    by_path: HashMap<String, u32>,
    imports: HashMap<String, Value>,
    /// The byte offsets where lines start, per file, read when first needed.
    line_starts: HashMap<u32, Rc<Vec<u64>>>,
}

/// The 1-based line and column of a byte offset in a compiled file.
pub fn line_col(file: u32, offset: u64) -> Option<(u32, u32)> {
    let starts = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let s = s.as_mut()?;
        if let Some(l) = s.line_starts.get(&file) {
            return Some(l.clone());
        }
        let path = abs(&s.root, s.files.get(file as usize)?.path);
        let data = std::fs::read(path).ok()?;
        let mut v = vec![0u64];
        for (i, b) in data.iter().enumerate() {
            if *b == b'\n' {
                v.push(i as u64 + 1);
            }
        }
        let v = Rc::new(v);
        s.line_starts.insert(file, v.clone());
        Some(v)
    })?;
    let line = match starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i - 1,
    };
    Some((line as u32 + 1, (offset - starts[line]) as u32 + 1))
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Installs a program whose files live under `root`. Must run on the thread
/// that evaluates, before anything else.
pub fn install(p: &'static Program, root: &str) {
    crate::symbol::install(p.symbols);
    crate::value::install_crates(p.crates);
    // `import` resolves symbolic links, so the root must be resolved too.
    let root = match std::fs::canonicalize(root) {
        Ok(r) => r.to_string_lossy().into_owned(),
        Err(_) => canon_path(root),
    };
    let mut by_path = HashMap::new();
    for (i, f) in p.files.iter().enumerate() {
        by_path.insert(abs(&root, f.path), i as u32);
    }
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            root,
            files: p.files,
            by_path,
            imports: HashMap::new(),
            line_starts: HashMap::new(),
        })
    });
}

fn abs(root: &str, rel: &str) -> String {
    if rel.starts_with('/') {
        rel.to_string()
    } else if root == "/" {
        format!("/{rel}")
    } else {
        format!("{root}/{rel}")
    }
}

/// The root directory of the installed program.
pub fn root() -> String {
    STATE.with(|s| s.borrow().as_ref().expect("no program installed").root.clone())
}

/// The absolute path of a compiled file.
pub fn file_path(file: u32) -> String {
    STATE.with(|s| {
        let s = s.borrow();
        let s = s.as_ref().expect("no program installed");
        abs(&s.root, s.files[file as usize].path)
    })
}

/// The directory of a compiled file.
pub fn file_dir(file: u32) -> String {
    crate::paths::dir_of(&file_path(file))
}

/// A path literal: `rel` resolved against the directory of `file`.
pub fn path_lit(file: u32, rel: &str) -> Value {
    Value::Path(Rc::from(crate::paths::join(&file_dir(file), rel)))
}

/// The leading literal of an interpolated path like `./a/${b}`: the
/// directory joined with `rel`, keeping a trailing slash, as Nix does for
/// the first segment of such a path.
pub fn path_prefix(file: u32, rel: &str) -> Value {
    let mut p = crate::paths::join(&file_dir(file), rel);
    if rel.ends_with('/') && !p.ends_with('/') {
        p.push('/');
    }
    Value::Path(Rc::from(p))
}

/// A `~/...` path literal: `rest` (which starts with `/`) below `$HOME`. As
/// the first segment of an interpolated path it keeps a trailing slash.
pub fn home_path(rest: &str, prefix: bool) -> Value {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut p = canon_path(&format!("{home}{rest}"));
    if prefix && rest.ends_with('/') && !p.ends_with('/') {
        p.push('/');
    }
    Value::Path(Rc::from(p))
}

/// Whether the program has a compiled file at an absolute path.
pub fn has_file(path: &str) -> bool {
    STATE.with(|s| s.borrow().as_ref().is_some_and(|s| s.by_path.contains_key(path)))
}

/// Resolves the argument of `import` to the compiled file it names. A
/// directory stands for its `default.nix`. The path as written is tried
/// first: the compiler registers files below linked directories under the
/// link, as Nix keeps such links in import paths. The fully resolved path
/// is the fallback.
fn resolve(path: &str) -> String {
    let p = std::path::Path::new(path);
    let file = if p.is_dir() {
        canon_path(&format!("{path}/default.nix"))
    } else {
        path.to_string()
    };
    if has_file(&file) {
        return file;
    }
    match std::fs::canonicalize(&file) {
        Ok(real) => real.to_string_lossy().into_owned(),
        Err(_) => file,
    }
}

/// `import path`.
pub fn import(path: &str) -> R<Value> {
    let path = resolve(path);
    if let Some(v) = STATE.with(|s| s.borrow().as_ref().and_then(|s| s.imports.get(&path).cloned())) {
        return Ok(v);
    }
    let entry = STATE.with(|s| {
        let s = s.borrow();
        let s = s.as_ref().expect("no program installed");
        s.by_path.get(&path).map(|&i| &s.files[i as usize])
    });
    let entry = entry.ok_or_else(|| {
        Error::eval(format!(
            "cannot import '{path}': the file is not part of the compiled program (only files under '{}' are)",
            root()
        ))
    })?;
    let code = entry
        .code
        .ok_or_else(|| Error::eval(format!("{} (in '{path}')", entry.error)))?;
    let v = run(code, &[], Value::Null).context(|| format!("while importing '{path}'"))?;
    STATE.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut() {
            s.imports.insert(path, v.clone());
        }
    });
    Ok(v)
}

/// Imports a file below the root by its relative path.
pub fn import_rel(rel: &str) -> R<Value> {
    import(&abs(&root(), rel))
}

/// Runs `f` on a thread with a large stack, as evaluation of nixpkgs
/// recurses deeply, and returns its result.
pub fn with_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name("eval".into())
        .stack_size(4 << 30)
        .spawn(f)
        .expect("cannot start the evaluation thread")
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e))
}

/// Evaluates the file `rel` and forces the result.
pub fn eval_file(rel: &str) -> R<Value> {
    force(&import_rel(rel)?)
}
