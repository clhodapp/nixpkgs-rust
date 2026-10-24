//! `nixpkgs-rust-compiler`: compiles every `.nix` file under a directory
//! into a Cargo workspace whose binary evaluates those files.
//!
//! ```text
//! nixpkgs-rust-compiler --root DIR --out DIR --runtime PATH
//!     [--runtime-root DIR] [--bin-name NAME] [--budget BYTES]
//! ```
//!
//! `--runtime` is the path of the `nixpkgs-rust-runtime` crate the generated
//! manifests depend on. `--runtime-root` is where the program looks for the
//! files when it runs (by default `--root` itself); the program reads them
//! for `readFile`, `readDir` and copies into the store, and runs the
//! compiled code for `import`. `--budget` is the size in bytes after which a
//! generated crate is closed and the next one started.

mod codegen;
mod emit;
mod free;
mod strings;

use std::path::{Path, PathBuf};

use codegen::Compiler;
use emit::{EmitOpts, FileRec, emit};

struct Args {
    root: PathBuf,
    out: PathBuf,
    runtime: String,
    runtime_root: Option<String>,
    bin_name: String,
    budget: usize,
    /// File extensions compiled besides `nix`.
    also_ext: Vec<String>,
    /// Files outside the root that `<NAME>` search path lookups find, such
    /// as Nix's own `nix/fetchurl.nix`, as (NAME, file).
    extra: Vec<(String, String)>,
    /// The optimisation level of the generated crates in release builds.
    opt_level: u8,
}

fn parse_args() -> Result<Args, String> {
    let mut root = None;
    let mut out = None;
    let mut runtime = None;
    let mut runtime_root = None;
    let mut bin_name = "nixpkgs-rust".to_string();
    let mut budget = 2_000_000;
    let mut also_ext = Vec::new();
    let mut extra = Vec::new();
    let mut opt_level = 0u8;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("option '{a}' needs an argument"));
        match a.as_str() {
            "--root" => root = Some(PathBuf::from(val()?)),
            "--out" => out = Some(PathBuf::from(val()?)),
            "--runtime" => runtime = Some(val()?),
            "--runtime-root" => runtime_root = Some(val()?),
            "--bin-name" => bin_name = val()?,
            "--budget" => budget = val()?.parse().map_err(|_| "--budget needs a number".to_string())?,
            "--also-ext" => also_ext.push(val()?),
            "--opt-level" => {
                opt_level = val()?
                    .parse()
                    .map_err(|_| "--opt-level needs a number from 0 to 3".to_string())?
            }
            "--extra" => {
                let v = val()?;
                let (n, p) = v
                    .split_once('=')
                    .ok_or_else(|| format!("--extra '{v}' is not NAME=FILE"))?;
                extra.push((n.to_string(), p.to_string()));
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(Args {
        root: root.ok_or("--root is required")?,
        out: out.ok_or("--out is required")?,
        runtime: runtime.ok_or("--runtime is required")?,
        runtime_root,
        bin_name,
        budget,
        also_ext,
        extra,
        opt_level,
    })
}

/// The files to compile below a root, by the path an import would name.
struct Tree {
    /// Files compiled on their own, as paths relative to the root. A file
    /// below a symbolic link to a directory is listed under the link: Nix
    /// does not resolve such links in import paths, so the file's relative
    /// path literals resolve against the directory as named.
    files: Vec<String>,
    /// Symbolic links to files, with the file they resolve to. Nix resolves
    /// the last component of an import path, so a link shares the code of
    /// its target.
    aliases: Vec<(String, String)>,
}

fn has_ext(p: &Path, also_ext: &[String]) -> bool {
    p.extension()
        .is_some_and(|x| x == "nix" || also_ext.iter().any(|a| x == a.as_str()))
}

/// Lists the files to compile below `root`.
fn walk(root: &Path, also_ext: &[String]) -> std::io::Result<Tree> {
    let mut t = Tree {
        files: Vec::new(),
        aliases: Vec::new(),
    };
    // The real directories on the current path, to stop at link cycles.
    let mut ancestors = vec![root.to_path_buf()];
    visit(root, Path::new(""), root, also_ext, &mut ancestors, &mut t)?;
    t.files.sort();
    t.aliases.sort();
    Ok(t)
}

fn visit(
    root: &Path,
    rel: &Path,
    real: &Path,
    also_ext: &[String],
    ancestors: &mut Vec<std::path::PathBuf>,
    t: &mut Tree,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(real)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let lexical = rel.join(&name);
        let ty = e.file_type()?;
        if ty.is_symlink() {
            let Ok(target) = std::fs::canonicalize(e.path()) else { continue };
            let Ok(target_rel) = target.strip_prefix(root) else { continue };
            if target.is_dir() {
                if !ancestors.contains(&target) {
                    ancestors.push(target.clone());
                    visit(root, &lexical, &target, also_ext, ancestors, t)?;
                    ancestors.pop();
                }
            } else if has_ext(&lexical, also_ext) {
                t.aliases.push((
                    lexical.to_string_lossy().into_owned(),
                    target_rel.to_string_lossy().into_owned(),
                ));
            }
        } else if ty.is_dir() {
            if name != ".git" {
                let real_dir = real.join(&name);
                ancestors.push(real_dir.clone());
                visit(root, &lexical, &real_dir, also_ext, ancestors, t)?;
                ancestors.pop();
            }
        } else if ty.is_file() && has_ext(&lexical, also_ext) {
            t.files.push(lexical.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// The path under which the program registers an `--extra` file.
fn extra_path(name: &str) -> String {
    format!("{}/{name}", nixpkgs_rust_runtime::corepkgs::PREFIX)
}

fn run(a: Args) -> Result<(), String> {
    let root = std::fs::canonicalize(&a.root).map_err(|e| format!("cannot read '{}': {e}", a.root.display()))?;
    let tree = walk(&root, &a.also_ext).map_err(|e| format!("cannot list '{}': {e}", root.display()))?;
    let mut c = Compiler::new(a.budget);
    let mut recs = Vec::with_capacity(tree.files.len() + tree.aliases.len() + a.extra.len());
    let mut failed = 0;
    let mut sources: Vec<(String, std::path::PathBuf)> =
        tree.files.iter().map(|f| (f.clone(), root.join(f))).collect();
    for (name, path) in &a.extra {
        sources.push((extra_path(name), std::path::PathBuf::from(path)));
    }
    let mut by_path = std::collections::HashMap::new();
    for (path, file) in &sources {
        let src = std::fs::read(file).map_err(|e| format!("cannot read '{}': {e}", file.display()))?;
        let src = strings::decode_lossless(&src);
        let result = c.compile_file(recs.len() as u32, &src);
        if let Err(e) = &result {
            failed += 1;
            eprintln!("note: {path}: {e}");
        }
        by_path.insert(path.clone(), result.clone());
        recs.push(FileRec {
            path: path.clone(),
            result,
        });
    }
    for (link, target) in &tree.aliases {
        let result = by_path
            .get(target)
            .cloned()
            .unwrap_or_else(|| Err(format!("'{link}' links to '{target}', which was not compiled")));
        recs.push(FileRec {
            path: link.clone(),
            result,
        });
    }
    let files = &recs;
    let root_s = a.runtime_root.clone().unwrap_or_else(|| root.to_string_lossy().into_owned());
    emit(
        &c,
        files,
        &EmitOpts {
            out: &a.out,
            runtime: &a.runtime,
            root: &root_s,
            bin_name: &a.bin_name,
            opt_level: a.opt_level,
        },
    )
    .map_err(|e| format!("cannot write '{}': {e}", a.out.display()))?;
    let bytes: usize = c.crates.iter().map(|k| k.src.len()).sum();
    eprintln!(
        "compiled {} files ({failed} with errors) into {} crates, {} MB of Rust",
        files.len(),
        c.crates.len() - 1,
        bytes / 1_000_000
    );
    Ok(())
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    // Deeply nested expressions recurse deeply in the code generator.
    let res = std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(move || run(args))
        .expect("cannot start the compiler thread")
        .join()
        .expect("the compiler thread panicked");
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
