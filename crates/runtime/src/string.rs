//! Strings and string context.
//!
//! A Nix string is a byte string plus a context: the set of store paths the
//! string refers to. `derivationStrict` turns the context of its arguments
//! into the derivation's inputs, so tracking it exactly is what makes
//! derivation paths match those of Nix.

use std::collections::BTreeSet;
use std::rc::Rc;

/// One element of a string context.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CtxElem {
    /// A store path that is not a derivation output: a source, or a `.drv`
    /// file referenced as a file.
    Opaque(Rc<str>),
    /// A derivation and its entire closure, as `drvPath` produces.
    DrvDeep(Rc<str>),
    /// One output of a derivation: the derivation path and the output name.
    Built(Rc<str>, Rc<str>),
}

pub type Context = BTreeSet<CtxElem>;

/// A string with its context. An empty context is stored as `None`.
#[derive(Clone, Default)]
pub struct NixStr {
    pub s: Box<[u8]>,
    pub ctx: Option<Box<Context>>,
}

impl NixStr {
    pub fn plain(s: impl Into<Box<[u8]>>) -> NixStr {
        NixStr { s: s.into(), ctx: None }
    }

    pub fn with_ctx(s: impl Into<Box<[u8]>>, ctx: Context) -> NixStr {
        NixStr {
            s: s.into(),
            ctx: if ctx.is_empty() { None } else { Some(Box::new(ctx)) },
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.s
    }

    /// The string as UTF-8, replacing invalid sequences.
    pub fn lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.s)
    }

    pub fn has_ctx(&self) -> bool {
        self.ctx.is_some()
    }

    pub fn ctx_iter(&self) -> impl Iterator<Item = &CtxElem> {
        self.ctx.iter().flat_map(|c| c.iter())
    }
}

/// Builds a string piece by piece, collecting context.
#[derive(Default)]
pub struct StrBuilder {
    pub s: Vec<u8>,
    pub ctx: Context,
}

impl StrBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, s: &NixStr) {
        self.s.extend_from_slice(&s.s);
        if let Some(c) = &s.ctx {
            self.ctx.extend(c.iter().cloned());
        }
    }

    pub fn push_bytes(&mut self, b: &[u8]) {
        self.s.extend_from_slice(b);
    }

    pub fn finish(self) -> NixStr {
        NixStr::with_ctx(self.s, self.ctx)
    }
}
