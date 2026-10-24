//! Display and encoding of string context elements.

use crate::string::CtxElem;

/// The element the way error messages name it.
pub fn display_elem(c: &CtxElem) -> String {
    match c {
        CtxElem::Opaque(p) => p.to_string(),
        CtxElem::DrvDeep(p) => p.to_string(),
        CtxElem::Built(drv, out) => format!("{drv}^{out}"),
    }
}

/// The element in the string encoding Nix uses internally: a plain store
/// path, `=` and a derivation path for a deep element, or `!out!drv` for one
/// output.
pub fn encode_elem(c: &CtxElem) -> String {
    match c {
        CtxElem::Opaque(p) => p.to_string(),
        CtxElem::DrvDeep(p) => format!("={p}"),
        CtxElem::Built(drv, out) => format!("!{out}!{drv}"),
    }
}
