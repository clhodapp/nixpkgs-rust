//! The runtime that Nix code compiled by `nixpkgs-rust-compiler` links
//! against: values and thunks, the operators and built-in functions, store
//! path computation, and derivations.
//!
//! Generated code uses the names in [`prelude`].

pub mod aterm;
pub mod attrs;
pub mod build;
pub mod builtins;
pub mod cli;
pub mod context;
pub mod corepkgs;
pub mod daemon;
pub mod derivation;
pub mod error;
pub mod install;
pub mod json;
pub mod langtests;
pub mod ops;
pub mod paths;
pub mod print;
pub mod program;
pub mod regex;
pub mod store;
pub mod string;
pub mod symbol;
pub mod toml;
pub mod value;
pub mod xml;

/// What generated code imports.
pub mod prelude {
    pub use crate::attrs::{Attr, Attrs};
    pub use crate::builtins::{builtins, global, pos_value};
    pub use crate::error::{Error, R};
    pub use crate::ops::*;
    pub use crate::program::{path_lit, path_prefix};
    pub use crate::string::NixStr;
    pub use crate::symbol::Sym;
    pub use crate::value::{
        CodeId, CrateTables, Formals, LambdaInfo, Pos, PosInfo, Value, call, force, run,
    };
}
