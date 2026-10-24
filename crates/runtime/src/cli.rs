//! The command line of a compiled program.
//!
//! ```text
//! <program> eval [FILE] [-A ATTRPATH] [--arg NAME VALUE] [--argstr NAME STRING] [--json|--xml]
//! <program> instantiate [FILE] -A ATTRPATH [...]
//! <program> build [FILE] -A ATTRPATH [-o LINK | --no-link] [-v] [...]
//! <program> install|remove [FILE] -A ATTRPATH... [--profile PROFILE] [-v] [...]
//! <program> list [--profile PROFILE]
//! <program> lang-tests
//! ```
//!
//! `FILE` is a compiled file, relative to the program's root (the default is
//! `default.nix`). Like `nix-instantiate`, a function with a `{ ... }`
//! pattern is called with the `--arg`/`--argstr` values, and so is every
//! such function met while following `-A`. A compiled program cannot parse
//! Nix at run time, so `--arg` accepts only literals (`true`, `false`,
//! `null`, integers, strings in double quotes, `{}`, `[]`) and `import PATH`
//! of a compiled file.

use std::rc::Rc;

use crate::attrs::{Attr, Attrs};
use crate::error::{Error, R};
use crate::program::{self, Program};
use crate::symbol::intern;
use crate::value::{Pos, Value, call, force};

/// Options shared by the evaluating commands.
#[derive(Clone, Default)]
pub struct EvalOpts {
    pub file: Option<String>,
    pub attr: Option<String>,
    pub args: Vec<(String, ArgValue)>,
    pub output: Output,
    /// For `build`: the `-o` link, unless `--no-link` was given.
    pub out_link: Option<String>,
    pub no_link: bool,
    /// For `build`: show the daemon's log.
    pub verbose: bool,
    /// Every `-A` given, for `install` and `remove`.
    pub attrs: Vec<String>,
    /// For `install`, `remove` and `list`: the profile.
    pub profile: Option<String>,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Output {
    #[default]
    Nix,
    Json,
    Xml,
}

#[derive(Clone)]
pub enum ArgValue {
    Str(String),
    Expr(String),
}

fn parse_opts(args: &[String]) -> Result<EvalOpts, String> {
    let mut o = EvalOpts::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("option '{a}' needs an argument"))
        };
        match a {
            "-A" | "--attr" => {
                let v = next()?;
                o.attrs.push(v.clone());
                o.attr = Some(v);
            }
            "-p" | "--profile" => o.profile = Some(next()?),
            "--arg" => {
                let n = next()?;
                let v = next()?;
                o.args.push((n, ArgValue::Expr(v)));
            }
            "--argstr" => {
                let n = next()?;
                let v = next()?;
                o.args.push((n, ArgValue::Str(v)));
            }
            "--json" => o.output = Output::Json,
            "--xml" => o.output = Output::Xml,
            "-o" | "--out-link" => o.out_link = Some(next()?),
            "--no-link" => o.no_link = true,
            "-v" | "--verbose" => o.verbose = true,
            "--strict" | "--eval" | "--no-location" | "--impure" => {}
            s if s.starts_with('-') => return Err(format!("unknown option '{s}'")),
            s => o.file = Some(s.to_string()),
        }
        i += 1;
    }
    Ok(o)
}

/// Evaluates the value of a `--arg` option.
fn arg_value(v: &ArgValue) -> R<Value> {
    match v {
        ArgValue::Str(s) => Ok(Value::str(s.as_bytes())),
        ArgValue::Expr(e) => {
            let e = e.trim();
            match e {
                "true" => return Ok(Value::Bool(true)),
                "false" => return Ok(Value::Bool(false)),
                "null" => return Ok(Value::Null),
                "{}" | "{ }" => return Ok(Value::attrs(Attrs::new())),
                "[]" | "[ ]" => return Ok(Value::list(Vec::new())),
                _ => {}
            }
            if let Ok(i) = e.parse::<i64>() {
                return Ok(Value::Int(i));
            }
            if e.len() >= 2 && e.starts_with('"') && e.ends_with('"') {
                return Ok(Value::str(e[1..e.len() - 1].as_bytes()));
            }
            if let Some(rest) = e.strip_prefix("import") {
                let p = rest.trim().trim_start_matches('(').trim_end_matches(')').trim();
                let abs = std::path::absolute(p).map_err(|err| Error::eval(format!("{err}")))?;
                return program::import(&abs.to_string_lossy());
            }
            Err(Error::eval(format!(
                "--arg '{e}': compiled programs accept only literals and 'import PATH' as --arg values"
            )))
        }
    }
}

/// Calls a function written with a `{ ... }` pattern with the given
/// arguments, as `nix-instantiate` does; other values are returned as they are.
pub fn auto_call(v: Value, args: &Rc<Attrs>) -> R<Value> {
    let f = force(&v)?;
    match &f {
        Value::Lambda(l) => {
            let Some(formals) = &l.info.formals else {
                return Ok(f);
            };
            let mut e = Vec::new();
            for (k, has_default, _) in formals.names {
                match args.get(*k) {
                    Some(x) => e.push(Attr {
                        k: *k,
                        v: x.clone(),
                        pos: Pos::default(),
                    }),
                    None if *has_default => {}
                    None => {
                        return Err(Error::eval(format!(
                            "cannot evaluate a function that has an argument without a value ('{}')",
                            crate::symbol::name(*k)
                        )));
                    }
                }
            }
            force(&call(&f, Value::attrs(Attrs::from_vec_first_wins(e)))?)
        }
        Value::Attrs(a) if a.contains(crate::symbol::known::functor) => {
            force(&call(&f, Value::attrs((**args).clone()))?)
        }
        _ => Ok(f),
    }
}

/// Follows an attribute path like `-A` does.
pub fn find_attr_path(mut v: Value, path: &str, args: &Rc<Attrs>) -> R<Value> {
    if path.is_empty() {
        return Ok(v);
    }
    for comp in path.split('.') {
        v = auto_call(v, args)?;
        match (&v, comp.parse::<usize>()) {
            (Value::List(l), Ok(i)) => {
                v = l
                    .get(i)
                    .cloned()
                    .ok_or_else(|| Error::eval(format!("list index {i} in selection path '{path}' is out of range")))?;
            }
            _ => {
                let a = crate::ops::force_attrs(&v)?;
                v = a.get(intern(comp)).cloned().ok_or_else(|| {
                    Error::eval(format!("attribute '{comp}' in selection path '{path}' not found"))
                })?;
            }
        }
    }
    auto_call(v, args)
}

/// Evaluates the value the options name.
pub fn eval_opts(o: &EvalOpts) -> R<Value> {
    let (v, args) = root_value(o)?;
    let v = auto_call(v, &args)?;
    find_attr_path(v, o.attr.as_deref().unwrap_or(""), &args)
}

/// The value of the file the options name, before any automatic call, and
/// the `--arg`/`--argstr` arguments.
pub fn root_value(o: &EvalOpts) -> R<(Value, Rc<Attrs>)> {
    let mut e = Vec::new();
    for (n, v) in &o.args {
        e.push(Attr {
            k: intern(n),
            v: arg_value(v)?,
            pos: Pos::default(),
        });
    }
    let args = Rc::new(Attrs::from_vec_last_wins(e));
    let file = o.file.clone().unwrap_or_else(|| "default.nix".to_string());
    let v = if file.starts_with('/') {
        program::import(&file)?
    } else {
        program::import_rel(&file)?
    };
    Ok((v, args))
}

/// Renders a value for `eval`.
pub fn render(v: &Value, output: Output) -> R<String> {
    Ok(match output {
        Output::Nix => crate::print::print_strict(v)?,
        Output::Json => {
            let mut ctx = crate::string::Context::new();
            crate::json::value_to_json(v, true, &mut ctx)?
        }
        Output::Xml => {
            let mut ctx = crate::string::Context::new();
            let s = crate::xml::value_to_xml(v, &mut ctx, true)?;
            s.trim_end_matches('\n').to_string()
        }
    })
}

/// The `.drv` path of a derivation value.
pub fn drv_path_of(v: &Value) -> R<String> {
    let a = crate::ops::force_attrs(v)?;
    if !crate::ops::is_derivation(&a)? {
        return Err(Error::eval("expression does not evaluate to a derivation"));
    }
    let d = a
        .get(crate::symbol::known::drvPath)
        .ok_or_else(|| Error::eval("derivation has no 'drvPath' attribute"))?;
    Ok(crate::ops::force_str(d)?.lossy().into_owned())
}

fn report(e: &Error) -> i32 {
    eprintln!("{e}");
    1
}

/// Runs `f` on an evaluation thread with the program installed.
pub fn on_eval_thread<T: Send + 'static>(
    p: &'static Program,
    root: String,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    program::with_big_stack(move || {
        program::install(p, &root);
        f()
    })
}

/// The `main` of a compiled program.
pub fn main(p: &'static Program, default_root: &'static str) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = std::env::var("NIXPKGS_RUST_ROOT").unwrap_or_else(|_| default_root.to_string());
    let code = match args.first().map(String::as_str) {
        Some("eval") => match parse_opts(&args[1..]) {
            Ok(o) => on_eval_thread(p, root, move || match eval_opts(&o).and_then(|v| render(&v, o.output)) {
                Ok(s) => {
                    println!("{s}");
                    0
                }
                Err(e) => report(&e),
            }),
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        },
        Some("instantiate") => match parse_opts(&args[1..]) {
            Ok(o) => on_eval_thread(p, root, move || match eval_opts(&o).and_then(|v| drv_path_of(&v)) {
                Ok(s) => {
                    println!("{s}");
                    0
                }
                Err(e) => report(&e),
            }),
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        },
        Some("build") => match parse_opts(&args[1..]) {
            Ok(o) => on_eval_thread(p, root, move || {
                let bo = crate::build::BuildOpts {
                    out_link: if o.no_link {
                        None
                    } else {
                        Some(o.out_link.clone().unwrap_or_else(|| "result".to_string()))
                    },
                    verbose: o.verbose,
                };
                match eval_opts(&o).and_then(|v| crate::build::build(&v, &bo)) {
                    Ok(out) => {
                        println!("{out}");
                        0
                    }
                    Err(e) => report(&e),
                }
            }),
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        },
        Some(cmd @ ("install" | "remove" | "list")) => {
            let cmd = cmd.to_string();
            match parse_opts(&args[1..]) {
                Ok(o) => on_eval_thread(p, root, move || {
                    let profile = o
                        .profile
                        .clone()
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(crate::install::default_profile);
                    if cmd == "list" {
                        for a in crate::install::installed(&profile) {
                            println!("{a}");
                        }
                        return 0;
                    }
                    if o.attrs.is_empty() {
                        eprintln!("error: name the packages with -A ATTRPATH");
                        return 2;
                    }
                    let res = if cmd == "install" {
                        crate::install::install(&o, &profile, &o.attrs, o.verbose)
                    } else {
                        crate::install::remove(&o, &profile, &o.attrs, o.verbose)
                    };
                    match res {
                        Ok(out) => {
                            println!("{} -> {out}", profile.display());
                            0
                        }
                        Err(e) => report(&e),
                    }
                }),
                Err(e) => {
                    eprintln!("error: {e}");
                    2
                }
            }
        }
        Some("lang-tests") => crate::langtests::run(p, &root),
        _ => {
            eprintln!(
                "usage: {} eval|instantiate|build [FILE] [-A ATTRPATH] [--arg NAME VALUE] [--argstr NAME STRING]",
                std::env::args().next().unwrap_or_default()
            );
            2
        }
    };
    std::process::exit(code);
}
