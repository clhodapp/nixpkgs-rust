//! A runner for the language tests of Nix (`tests/functional/lang`), for a
//! program compiled from that directory.
//!
//! It mirrors `tests/functional/lang.sh`: each `eval-okay-*` test is
//! evaluated strictly and its output compared with the `.exp` file (or
//! `.exp.xml` for the XML tests), with `NIX_PATH=lang/dir3:lang/dir4`,
//! `HOME=/fake-home` and `TEST_VAR=foo`, run from the parent of the test
//! directory with that directory printed as `/pwd`. Each `eval-fail-*` test
//! must fail. Standard error is not compared.

use std::path::Path;

use crate::cli::{ArgValue, EvalOpts, Output, eval_opts, on_eval_thread, render};
use crate::program::Program;

enum Outcome {
    Pass,
    Fail(String),
    Skip(String),
}

/// Reads the `.flags` file of a test into evaluation options, or says why
/// the test cannot run.
fn flags(dir: &Path, name: &str, o: &mut EvalOpts, includes: &mut Vec<String>) -> Result<(), String> {
    let Ok(text) = std::fs::read_to_string(dir.join(format!("{name}.flags"))) else {
        return Ok(());
    };
    let text: String = text.lines().map(|l| l.split('#').next().unwrap_or("")).collect::<Vec<_>>().join(" ");
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        match words[i] {
            "-I" => {
                includes.push(words.get(i + 1).ok_or("-I without a value")?.to_string());
                i += 2;
            }
            "--arg" => {
                o.args.push((words[i + 1].to_string(), ArgValue::Expr(words[i + 2].to_string())));
                i += 3;
            }
            "--argstr" => {
                o.args.push((words[i + 1].to_string(), ArgValue::Str(words[i + 2].to_string())));
                i += 3;
            }
            "-A" => {
                o.attr = Some(words[i + 1].to_string());
                i += 2;
            }
            w if w.starts_with("--lint-") => {
                if words.get(i + 1) == Some(&"fatal") {
                    return Err(format!("needs the '{w}' lint, which compiled programs do not implement"));
                }
                i += 2;
            }
            "--impure" | "--eval" | "--strict" => i += 1,
            "--extra-experimental-features" => {
                return Err(format!("needs experimental feature '{}'", words.get(i + 1).unwrap_or(&"")));
            }
            w => return Err(format!("unsupported flag '{w}'")),
        }
    }
    Ok(())
}

fn run_one(p: &'static Program, root: &str, dir: &Path, name: &str, okay: bool) -> Outcome {
    let mut o = EvalOpts {
        file: Some(format!("{name}.nix")),
        ..Default::default()
    };
    let mut includes = Vec::new();
    if let Err(why) = flags(dir, name, &mut o, &mut includes) {
        return Outcome::Skip(why);
    }
    let xml = dir.join(format!("{name}.exp.xml"));
    let expected = if okay {
        if xml.exists() {
            o.output = Output::Xml;
            std::fs::read_to_string(&xml).ok()
        } else {
            match std::fs::read_to_string(dir.join(format!("{name}.exp"))) {
                Ok(s) => Some(s),
                Err(_) => return Outcome::Skip("no expected output".to_string()),
            }
        }
    } else {
        None
    };
    // SAFETY: the runner is single-threaded apart from the evaluation thread
    // it starts after setting these, and waits for.
    unsafe {
        std::env::set_var("NIXPKGS_RUST_INCLUDE", includes.join(":"));
    }
    let result = on_eval_thread(p, root.to_string(), move || {
        eval_opts(&o).and_then(|v| render(&v, o.output)).map_err(|e| e.to_string())
    });
    let pwd = dir.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    match (okay, result) {
        (true, Ok(out)) => {
            let out = out.replace(&pwd, "/pwd");
            let exp = expected.unwrap_or_default();
            if out.trim_end() == exp.trim_end() {
                Outcome::Pass
            } else {
                Outcome::Fail(format!("expected:\n{}\ngot:\n{}", exp.trim_end(), out.trim_end()))
            }
        }
        (true, Err(e)) => Outcome::Fail(format!("evaluation failed:\n{e}")),
        (false, Err(_)) => Outcome::Pass,
        (false, Ok(out)) => Outcome::Fail(format!("evaluated, but should have failed, to:\n{out}")),
    }
}

/// Runs every language test in the program's root directory. Returns the
/// process exit code: 0 when every test that could run passed.
pub fn run(p: &'static Program, root: &str) -> i32 {
    let dir = Path::new(root);
    let Some(parent) = dir.parent() else {
        eprintln!("the test directory has no parent directory");
        return 2;
    };
    let only: Option<String> = std::env::var("LANG_TESTS_ONLY").ok();
    // SAFETY: no other thread is running yet.
    unsafe {
        std::env::set_current_dir(parent).expect("cannot enter the parent of the test directory");
        std::env::set_var("NIX_PATH", "lang/dir3:lang/dir4");
        std::env::set_var("HOME", "/fake-home");
        std::env::set_var("TEST_VAR", "foo");
    }
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("cannot read the test directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| (n.starts_with("eval-okay-") || n.starts_with("eval-fail-")) && n.ends_with(".nix"))
        .map(|n| n.trim_end_matches(".nix").to_string())
        .collect();
    names.sort();
    let (mut pass, mut fail, mut skip) = (0, 0, 0);
    for name in names {
        if let Some(o) = &only
            && !name.contains(o.as_str())
        {
            continue;
        }
        let okay = name.starts_with("eval-okay-");
        if okay && dir.join(format!("{name}.exp-disabled")).exists() {
            continue;
        }
        if std::env::var_os("LANG_TESTS_VERBOSE").is_some() {
            eprintln!("running {name}");
        }
        match run_one(p, root, dir, &name, okay) {
            Outcome::Pass => pass += 1,
            Outcome::Skip(why) => {
                skip += 1;
                println!("SKIP {name}: {why}");
            }
            Outcome::Fail(why) => {
                fail += 1;
                println!("FAIL {name}\n{why}\n");
            }
        }
    }
    println!("{pass} passed, {fail} failed, {skip} skipped");
    if fail == 0 { 0 } else { 1 }
}
