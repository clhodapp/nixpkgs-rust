//! Files Nix provides under the `nix/` search path prefix, such as
//! `<nix/fetchurl.nix>`, which nixpkgs uses to fetch its bootstrap tools.
//!
//! The compiler compiles these from the Nix source (its `--extra` option)
//! and registers each under [`PREFIX`]; a search path lookup that the
//! search path itself does not satisfy falls back to them.

use std::rc::Rc;

use crate::value::Value;

/// The directory the extra files are registered under. It does not exist
/// on disk.
pub const PREFIX: &str = "/__corepkgs__";

/// The path a `<name>` lookup resolves to when the program carries the file.
pub fn find(name: &str) -> Option<Value> {
    let p = format!("{PREFIX}/{name}");
    crate::program::has_file(&p).then(|| Value::Path(Rc::from(p)))
}
