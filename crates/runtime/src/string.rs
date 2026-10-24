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

/// The bytes of a string: borrowed from the program when they come from a
/// string literal, so that evaluating a literal copies nothing, or owned.
#[derive(Clone)]
pub enum Bytes {
    Static(&'static [u8]),
    Owned(Box<[u8]>),
}

impl Default for Bytes {
    fn default() -> Self {
        Bytes::Static(b"")
    }
}

impl std::ops::Deref for Bytes {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Static(s) => s,
            Bytes::Owned(s) => s,
        }
    }
}

impl PartialEq for Bytes {
    fn eq(&self, other: &Bytes) -> bool {
        **self == **other
    }
}

impl Eq for Bytes {}

impl PartialOrd for Bytes {
    fn partial_cmp(&self, other: &Bytes) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bytes {
    fn cmp(&self, other: &Bytes) -> std::cmp::Ordering {
        (**self).cmp(&**other)
    }
}

impl From<Box<[u8]>> for Bytes {
    fn from(b: Box<[u8]>) -> Bytes {
        Bytes::Owned(b)
    }
}

impl From<Bytes> for Box<[u8]> {
    fn from(b: Bytes) -> Box<[u8]> {
        match b {
            Bytes::Static(s) => s.into(),
            Bytes::Owned(s) => s,
        }
    }
}

/// A string with its context. An empty context is stored as `None`.
#[derive(Clone, Default)]
pub struct NixStr {
    pub s: Bytes,
    pub ctx: Option<Box<Context>>,
}

impl NixStr {
    pub fn plain(s: impl Into<Box<[u8]>>) -> NixStr {
        NixStr {
            s: Bytes::Owned(s.into()),
            ctx: None,
        }
    }

    /// A string literal of the program, without copying its bytes.
    pub fn literal(s: &'static [u8]) -> NixStr {
        NixStr {
            s: Bytes::Static(s),
            ctx: None,
        }
    }

    pub fn with_ctx(s: impl Into<Box<[u8]>>, ctx: Context) -> NixStr {
        NixStr {
            s: Bytes::Owned(s.into()),
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
