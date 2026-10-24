//! Interned attribute names.
//!
//! Every attribute name and identifier the evaluator handles is a [`Sym`], an
//! index into a per-thread table of names. The compiler assigns the ids of
//! every name that appears in the source it compiles, writes the table into
//! the generated program, and the program registers that table with
//! [`install`] before evaluating anything. The table starts with
//! [`PREDEFINED`], the names this runtime refers to by constant, so the ids in
//! [`known`] mean the same thing in every program.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// An interned name. Two symbols are equal exactly when their names are.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Sym(pub u32);

macro_rules! predefined {
    ($($id:ident = $s:expr;)*) => {
        /// The names the runtime uses by constant, in id order.
        pub const PREDEFINED: &[&str] = &[$($s),*];
        /// Constants for the predefined names.
        #[allow(non_upper_case_globals)]
        pub mod known {
            use super::Sym;
            predefined!(@consts 0u32, $($id)*);
        }
    };
    (@consts $n:expr, $id:ident $($rest:ident)*) => {
        pub const $id: Sym = Sym($n);
        predefined!(@consts $n + 1u32, $($rest)*);
    };
    (@consts $n:expr,) => {};
}

predefined! {
    outPath = "outPath";
    type_ = "type";
    functor = "__functor";
    name = "name";
    drvPath = "drvPath";
    value = "value";
    success = "success";
    key = "key";
    startSet = "startSet";
    operator = "operator";
    file = "file";
    line = "line";
    column = "column";
    outputs = "outputs";
    out = "out";
    system = "system";
    builder = "builder";
    args = "args";
    ignoreNulls = "__ignoreNulls";
    structuredAttrs = "__structuredAttrs";
    outputHash = "outputHash";
    outputHashAlgo = "outputHashAlgo";
    outputHashMode = "outputHashMode";
    outputName = "outputName";
    drvAttrs = "drvAttrs";
    all = "all";
    right = "right";
    wrong = "wrong";
    path = "path";
    recursive = "recursive";
    sha256 = "sha256";
    filter = "filter";
    prefix = "prefix";
    toString = "__toString";
    version = "version";
    contentAddressed = "__contentAddressed";
    impure = "__impure";
    json = "__json";
    url = "url";
    hash = "hash";
    rev = "rev";
    submodules = "submodules";
    shallow = "shallow";
    ref_ = "ref";
    allRefs = "allRefs";
    narHash = "narHash";
    expr = "expr";
    lineSym = "__line";
    meta = "meta";
    regular = "regular";
    directory = "directory";
    symlink = "symlink";
    unknown = "unknown";
    outputSpecified = "outputSpecified";
    outputChecks = "outputChecks";
    exportReferencesGraph = "exportReferencesGraph";
    allowedReferences = "allowedReferences";
    allowedRequisites = "allowedRequisites";
    disallowedReferences = "disallowedReferences";
    disallowedRequisites = "disallowedRequisites";
    maxSize = "maxSize";
    maxClosureSize = "maxClosureSize";
    ignoreSelfRefs = "ignoreSelfRefs";
    head = "head";
    tail = "tail";
    nixPath = "nixPath";
    builtins = "builtins";
    true_ = "true";
    false_ = "false";
    null = "null";
    import = "import";
    abort = "abort";
    throw = "throw";
    derivation = "derivation";
    unpack = "unpack";
    executable = "executable";
}

struct Interner {
    map: HashMap<Rc<str>, Sym>,
    names: Vec<Rc<str>>,
}

impl Interner {
    fn new() -> Self {
        let mut i = Interner {
            map: HashMap::new(),
            names: Vec::new(),
        };
        for n in PREDEFINED {
            i.intern(n);
        }
        i
    }

    fn intern(&mut self, s: &str) -> Sym {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = Sym(self.names.len() as u32);
        let rc: Rc<str> = Rc::from(s);
        self.names.push(rc.clone());
        self.map.insert(rc, id);
        id
    }
}

thread_local! {
    static INTERNER: RefCell<Interner> = RefCell::new(Interner::new());
}

/// Registers a program's symbol table. The table must start with
/// [`PREDEFINED`] and must be installed before any other name is interned on
/// this thread, so that the ids the compiler assigned are the ids in use.
pub fn install(table: &[&str]) {
    INTERNER.with(|i| {
        let mut i = i.borrow_mut();
        assert!(
            i.names.len() == PREDEFINED.len(),
            "the symbol table is installed after other names were interned"
        );
        for (n, expected) in table.iter().enumerate() {
            let got = i.intern(expected);
            assert_eq!(got.0 as usize, n, "symbol table out of order at {expected:?}");
        }
    });
}

/// Returns the symbol for `s`, adding it to the table if it is new.
pub fn intern(s: &str) -> Sym {
    INTERNER.with(|i| i.borrow_mut().intern(s))
}

/// Returns the symbol for `s` without adding it.
pub fn lookup(s: &str) -> Option<Sym> {
    INTERNER.with(|i| i.borrow().map.get(s).copied())
}

/// Returns the name of a symbol.
pub fn name(s: Sym) -> Rc<str> {
    INTERNER.with(|i| i.borrow().names[s.0 as usize].clone())
}

impl std::fmt::Display for Sym {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&name(*self))
    }
}
