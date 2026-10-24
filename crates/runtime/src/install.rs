//! Installing packages into a profile, as `nix-env --install` does.
//!
//! A profile is a symbolic link to the current *generation*, itself a link
//! (`<profile>-<n>-link`) to a store path that merges the installed packages
//! with the `buildEnv` function of nixpkgs. Each generation records the
//! attribute paths it contains in `<profile>-<n>-link.attrs`, one per line,
//! so that the next install or removal can rebuild the environment from the
//! same nixpkgs. Generation links are registered as garbage-collector roots.

use std::path::{Path, PathBuf};

use crate::attrs::{Attr, Attrs};
use crate::build::{BuildOpts, build};
use crate::cli::{EvalOpts, auto_call, find_attr_path, root_value};
use crate::error::{Error, R};
use crate::symbol::{intern, known};
use crate::value::{Pos, Value, call};

/// The profile used when none is given: `$NIXPKGS_RUST_PROFILE`, or
/// `$XDG_STATE_HOME/nixpkgs-rust/profile` (`~/.local/state/...`).
pub fn default_profile() -> PathBuf {
    if let Ok(p) = std::env::var("NIXPKGS_RUST_PROFILE") {
        return PathBuf::from(p);
    }
    let state = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"));
    state.join("nixpkgs-rust/profile")
}

fn io(what: &Path, e: std::io::Error) -> Box<Error> {
    Error::eval(format!("{}: {e}", what.display()))
}

fn gen_link(profile: &Path, n: u64) -> PathBuf {
    let name = profile.file_name().unwrap_or_default().to_string_lossy();
    profile.with_file_name(format!("{name}-{n}-link"))
}

/// The current generation number, if the profile exists.
fn current_generation(profile: &Path) -> Option<u64> {
    let target = std::fs::read_link(profile).ok()?;
    let name = target.file_name()?.to_string_lossy().into_owned();
    let prefix = format!("{}-", profile.file_name()?.to_string_lossy());
    name.strip_prefix(&prefix)?.strip_suffix("-link")?.parse().ok()
}

/// The attribute paths installed in the current generation.
pub fn installed(profile: &Path) -> Vec<String> {
    let Some(n) = current_generation(profile) else { return Vec::new() };
    let mut f = gen_link(profile, n).into_os_string();
    f.push(".attrs");
    std::fs::read_to_string(f)
        .map(|s| s.lines().filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// Builds an environment of `attrs` and makes it the new generation of the
/// profile. Returns the environment's store path.
pub fn switch(o: &EvalOpts, profile: &Path, attrs: &[String], verbose: bool) -> R<String> {
    let (root, args) = root_value(o)?;
    let pkgs = auto_call(root, &args)?;
    let mut paths = Vec::new();
    for a in attrs {
        let v = find_attr_path(pkgs.clone(), a, &args)?;
        if !crate::ops::is_derivation(&*crate::ops::force_attrs(&v)?)? {
            return Err(Error::eval(format!("'{a}' is not a package")));
        }
        paths.push(v);
    }
    let build_env = crate::ops::select(&pkgs, intern("buildEnv"))?;
    let env_args = Value::attrs(Attrs::from_vec_first_wins(vec![
        Attr {
            k: known::name,
            v: Value::str(&b"user-environment"[..]),
            pos: Pos::default(),
        },
        Attr {
            k: intern("paths"),
            v: Value::list(paths),
            pos: Pos::default(),
        },
    ]));
    let env = call(&build_env, env_args)?;
    let out = build(
        &env,
        &BuildOpts {
            out_link: None,
            verbose,
        },
    )?;

    if let Some(dir) = profile.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    let n = current_generation(profile).unwrap_or(0) + 1;
    let link = std::path::absolute(gen_link(profile, n)).map_err(|e| io(profile, e))?;
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&out, &link).map_err(|e| io(&link, e))?;
    let mut d = crate::daemon::Daemon::connect().map_err(Error::eval)?;
    d.add_indirect_root(&link.to_string_lossy()).map_err(Error::eval)?;
    let mut list = link.clone().into_os_string();
    list.push(".attrs");
    std::fs::write(&list, attrs.join("\n") + "\n").map_err(|e| io(Path::new(&list), e))?;
    // Switch atomically: a new link renamed over the profile.
    let tmp = profile.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(link.file_name().unwrap(), &tmp).map_err(|e| io(&tmp, e))?;
    std::fs::rename(&tmp, profile).map_err(|e| io(profile, e))?;
    Ok(out)
}

/// `install`: adds attribute paths to the profile.
pub fn install(o: &EvalOpts, profile: &Path, new: &[String], verbose: bool) -> R<String> {
    let mut attrs = installed(profile);
    for a in new {
        if !attrs.contains(a) {
            attrs.push(a.clone());
        }
    }
    switch(o, profile, &attrs, verbose)
}

/// `remove`: drops attribute paths from the profile.
pub fn remove(o: &EvalOpts, profile: &Path, gone: &[String], verbose: bool) -> R<String> {
    let attrs: Vec<String> = installed(profile).into_iter().filter(|a| !gone.contains(a)).collect();
    switch(o, profile, &attrs, verbose)
}
