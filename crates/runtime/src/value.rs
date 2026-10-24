//! Values, thunks and function application.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use crate::attrs::Attrs;
use crate::error::{Error, Kind, R};
use crate::string::NixStr;
use crate::symbol::{Sym, known};

/// A Nix value. Cloning is cheap: every payload larger than a word is
/// reference counted.
#[derive(Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<NixStr>),
    /// An absolute, canonical path.
    Path(Rc<str>),
    Attrs(Rc<Attrs>),
    List(Rc<Vec<Value>>),
    Lambda(Rc<Lambda>),
    Prim(Rc<PrimApp>),
    Thunk(Rc<Thunk>),
}

/// The signature of every generated function. Thunk bodies ignore the
/// argument; lambda bodies receive the function's argument. The slice holds
/// the values the function captured when its thunk or lambda was created.
pub type Code = fn(&[Value], Value) -> R<Value>;

/// Names a generated function: the crate it lives in (upper 12 bits) and its
/// index in that crate's table (lower 20 bits). Generated crates refer to each
/// other's functions only through these ids, so no generated crate depends on
/// another and they compile in parallel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CodeId(pub u32);

/// A source position: the id of a compiled file plus one (upper 24 bits) and
/// a byte offset into it (lower 40 bits). `Pos(0)` is "no position". Lines
/// and columns are computed from the file when they are needed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Pos(pub u64);

impl Pos {
    pub const fn new(file: u32, offset: u64) -> Pos {
        Pos(((file as u64 + 1) << 40) | offset)
    }
}

/// A resolved source position: a file id and a 1-based line and column.
#[derive(Clone, Copy, Debug)]
pub struct PosInfo {
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

/// The table of generated functions one generated crate contributes.
pub struct CrateTables {
    pub codes: &'static [Code],
}

static CRATES: OnceLock<&'static [&'static CrateTables]> = OnceLock::new();

/// Registers the generated crates. Index 0 of `crates` is crate id 0, which
/// holds no code; generated crates are numbered from 1. Installing the same
/// program again (on another evaluation thread) does nothing.
pub fn install_crates(crates: &'static [&'static CrateTables]) {
    let _ = CRATES.set(crates);
}

#[inline]
fn tables(id: u32) -> &'static CrateTables {
    CRATES.get().expect("generated crates are not installed")[(id >> 20) as usize]
}

/// Runs a generated function.
#[inline]
pub fn run(id: CodeId, env: &[Value], arg: Value) -> R<Value> {
    (tables(id.0).codes[(id.0 & 0xfffff) as usize])(env, arg)
}

/// Resolves a source position to a line and column.
pub fn pos_info(p: Pos) -> Option<PosInfo> {
    if p.0 == 0 {
        return None;
    }
    let file = ((p.0 >> 40) - 1) as u32;
    let (line, column) = crate::program::line_col(file, p.0 & 0xff_ffff_ffff)?;
    Some(PosInfo { file, line, column })
}

/// The formal arguments of a lambda written with a `{ ... }` pattern.
pub struct Formals {
    /// Each formal, whether it has a default, and where it is written,
    /// sorted by symbol.
    pub names: &'static [(Sym, bool, Pos)],
    pub ellipsis: bool,
}

/// What the compiler knows about one lambda in the source: its parameter
/// (the name of `x: ...`, or the `@` name of a pattern), its pattern, and
/// where it is.
pub struct LambdaInfo {
    pub arg: Option<Sym>,
    pub formals: Option<Formals>,
    pub pos: Pos,
}

/// The values a thunk or lambda captured. Up to two are stored in place,
/// which covers 85% of the thunks and lambdas compiled from nixpkgs (59%
/// capture one value), so that most take one allocation instead of two.
pub enum Env {
    E0,
    E1([Value; 1]),
    E2([Value; 2]),
    Heap(Box<[Value]>),
}

impl Env {
    #[inline]
    pub fn as_slice(&self) -> &[Value] {
        match self {
            Env::E0 => &[],
            Env::E1(v) => v,
            Env::E2(v) => v,
            Env::Heap(v) => v,
        }
    }
}

impl<const N: usize> From<[Value; N]> for Env {
    #[inline]
    fn from(a: [Value; N]) -> Env {
        let mut it = a.into_iter();
        match N {
            0 => Env::E0,
            1 => Env::E1([it.next().unwrap()]),
            2 => Env::E2([it.next().unwrap(), it.next().unwrap()]),
            _ => Env::Heap(it.collect()),
        }
    }
}

/// A closure: a generated function plus the values it captured.
pub struct Lambda {
    pub code: CodeId,
    pub env: Env,
    pub info: &'static LambdaInfo,
}

/// A built-in function.
pub struct PrimOp {
    pub name: &'static str,
    pub arity: usize,
    pub f: fn(&[Value]) -> R<Value>,
}

/// A built-in function applied to fewer arguments than its arity.
pub struct PrimApp {
    pub op: &'static PrimOp,
    pub args: Vec<Value>,
}

/// The state of a thunk.
pub enum ThunkState {
    /// Allocated so that recursive bindings can refer to it, not yet filled.
    Blank,
    /// A generated function and its captured values.
    Code(CodeId, Env),
    /// The application of a function to an argument.
    App(Value, Value),
    /// The selection of an attribute.
    Sel(Value, Sym),
    /// A runtime closure.
    Native(Rc<dyn Fn() -> R<Value>>),
    /// Being forced.
    Running,
    /// Forced; the value is in weak head normal form.
    Done(Value),
}

/// A suspended computation that runs at most once to completion. When it
/// fails, it goes back to its unforced state, so forcing it again repeats the
/// failure (which is what `builtins.tryEval` relies on).
pub struct Thunk(pub RefCell<ThunkState>);

impl Value {
    #[inline]
    pub fn thunk(code: CodeId, env: Env) -> Value {
        Value::Thunk(Rc::new(Thunk(RefCell::new(ThunkState::Code(code, env)))))
    }

    /// A thunk to be filled with [`Value::init`].
    #[inline]
    pub fn blank() -> Value {
        Value::Thunk(Rc::new(Thunk(RefCell::new(ThunkState::Blank))))
    }

    /// `n` thunks to be filled with [`Value::init`], for the bindings of a
    /// large `let` or recursive set.
    pub fn blanks(n: usize) -> Vec<Value> {
        (0..n).map(|_| Value::blank()).collect()
    }

    /// Fills a thunk made by [`Value::blank`].
    #[inline]
    pub fn init(&self, code: CodeId, env: Env) {
        match self {
            Value::Thunk(t) => *t.0.borrow_mut() = ThunkState::Code(code, env),
            _ => unreachable!("init on a value that is not a blank thunk"),
        }
    }

    /// Fills a blank thunk with a value that is already known.
    pub fn init_value(&self, v: Value) {
        match self {
            Value::Thunk(t) => {
                *t.0.borrow_mut() = match v {
                    Value::Thunk(_) => ThunkState::App(Value::Prim(Rc::new(PrimApp { op: &ID, args: vec![] })), v),
                    v => ThunkState::Done(v),
                }
            }
            _ => unreachable!("init on a value that is not a blank thunk"),
        }
    }

    /// The lazy application `f x`.
    #[inline]
    pub fn app(f: Value, x: Value) -> Value {
        Value::Thunk(Rc::new(Thunk(RefCell::new(ThunkState::App(f, x)))))
    }

    /// The lazy selection `v.k`.
    #[inline]
    pub fn sel(v: Value, k: Sym) -> Value {
        Value::Thunk(Rc::new(Thunk(RefCell::new(ThunkState::Sel(v, k)))))
    }

    /// A lazy value computed by a runtime closure.
    pub fn native(f: impl Fn() -> R<Value> + 'static) -> Value {
        Value::Thunk(Rc::new(Thunk(RefCell::new(ThunkState::Native(Rc::new(f))))))
    }

    pub fn lambda(code: CodeId, env: Env, info: &'static LambdaInfo) -> Value {
        Value::Lambda(Rc::new(Lambda { code, env, info }))
    }

    pub fn prim(op: &'static PrimOp) -> Value {
        Value::Prim(Rc::new(PrimApp { op, args: Vec::new() }))
    }

    pub fn attrs(a: Attrs) -> Value {
        Value::Attrs(Rc::new(a))
    }

    pub fn list(l: Vec<Value>) -> Value {
        Value::List(Rc::new(l))
    }

    pub fn str(s: impl Into<Box<[u8]>>) -> Value {
        Value::Str(Rc::new(NixStr::plain(s)))
    }

    /// A string literal of the program; its bytes are not copied.
    #[inline]
    pub fn lit(s: &'static [u8]) -> Value {
        Value::Str(Rc::new(NixStr::literal(s)))
    }

    pub fn string(s: NixStr) -> Value {
        Value::Str(Rc::new(s))
    }

    /// The type name `builtins.typeOf` reports, for a forced value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Path(_) => "path",
            Value::Attrs(_) => "set",
            Value::List(_) => "list",
            Value::Lambda(_) | Value::Prim(_) => "lambda",
            Value::Thunk(_) => "thunk",
        }
    }

    /// The type as Nix's error messages phrase it ("a string", "a set").
    pub fn show_type(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "a Boolean",
            Value::Int(_) => "an integer",
            Value::Float(_) => "a float",
            Value::Str(_) => "a string",
            Value::Path(_) => "a path",
            Value::Attrs(_) => "a set",
            Value::List(_) => "a list",
            Value::Lambda(_) => "a function",
            Value::Prim(p) if p.args.is_empty() => "a built-in function",
            Value::Prim(_) => "a partially applied built-in function",
            Value::Thunk(_) => "a thunk",
        }
    }

    pub fn is_function(&self) -> bool {
        matches!(self, Value::Lambda(_) | Value::Prim(_))
    }
}

static ID: PrimOp = PrimOp {
    name: "id",
    arity: 1,
    f: |a| force(&a[0]),
};

impl Thunk {
    fn force(&self) -> R<Value> {
        if let ThunkState::Done(v) = &*self.0.borrow() {
            return Ok(v.clone());
        }
        let st = std::mem::replace(&mut *self.0.borrow_mut(), ThunkState::Running);
        let res = match &st {
            ThunkState::Code(id, env) => run(*id, env.as_slice(), Value::Null),
            ThunkState::App(f, x) => call(f, x.clone()),
            ThunkState::Sel(v, k) => crate::ops::select(v, *k),
            ThunkState::Native(f) => f(),
            ThunkState::Running => {
                *self.0.borrow_mut() = ThunkState::Running;
                return Err(Error::new(Kind::InfiniteRecursion, "infinite recursion encountered"));
            }
            ThunkState::Blank => unreachable!("forced a blank thunk"),
            ThunkState::Done(_) => unreachable!(),
        };
        match res.and_then(|v| force(&v)) {
            Ok(v) => {
                *self.0.borrow_mut() = ThunkState::Done(v.clone());
                Ok(v)
            }
            Err(e) => {
                *self.0.borrow_mut() = st;
                Err(e)
            }
        }
    }

    /// Whether the thunk has been forced successfully.
    pub fn is_done(&self) -> bool {
        matches!(&*self.0.borrow(), ThunkState::Done(_))
    }
}

/// Evaluates a value to weak head normal form.
#[inline]
pub fn force(v: &Value) -> R<Value> {
    match v {
        Value::Thunk(t) => t.force(),
        _ => Ok(v.clone()),
    }
}

/// Applies a function to an argument. The result may be unevaluated.
pub fn call(f: &Value, arg: Value) -> R<Value> {
    let f = force(f)?;
    let _d = depth_guard()?;
    match &f {
        Value::Lambda(l) => run(l.code, l.env.as_slice(), arg),
        Value::Prim(p) => {
            if p.args.len() + 1 == p.op.arity {
                if p.args.is_empty() {
                    (p.op.f)(std::slice::from_ref(&arg))
                } else {
                    let mut args = Vec::with_capacity(p.op.arity);
                    args.extend(p.args.iter().cloned());
                    args.push(arg);
                    (p.op.f)(&args)
                }
            } else {
                let mut args = Vec::with_capacity(p.op.arity);
                args.extend(p.args.iter().cloned());
                args.push(arg);
                Ok(Value::Prim(Rc::new(PrimApp { op: p.op, args })))
            }
        }
        Value::Attrs(a) => match a.get(known::functor) {
            Some(functor) => {
                let g = call(functor, f.clone())?;
                call(&g, arg)
            }
            None => Err(Error::eval("attempt to call something which is not a function but a set")),
        },
        other => Err(Error::eval(format!(
            "attempt to call something which is not a function but {}",
            other.show_type()
        ))),
    }
}

/// Applies a function to several arguments in turn.
pub fn call_n(f: &Value, args: &[Value]) -> R<Value> {
    let mut f = f.clone();
    for a in args {
        f = call(&f, a.clone())?;
    }
    Ok(f)
}

/// Evaluates a value and its contents completely, as `builtins.deepSeq`
/// does. A set or list met a second time is not walked again, so cyclic
/// values terminate.
pub fn deep_force(v: &Value) -> R<Value> {
    let mut seen = std::collections::HashSet::new();
    deep_rec(v, &mut seen)
}

fn deep_rec(v: &Value, seen: &mut std::collections::HashSet<usize>) -> R<Value> {
    let v = force(v)?;
    let _d = depth_guard()?;
    match &v {
        Value::Attrs(a) => {
            if seen.insert(Rc::as_ptr(a) as *const u8 as usize) {
                for at in a.iter() {
                    deep_rec(&at.v, seen)?;
                }
            }
        }
        Value::List(l) => {
            if seen.insert(Rc::as_ptr(l) as *const u8 as usize) {
                for x in l.iter() {
                    deep_rec(x, seen)?;
                }
            }
        }
        _ => {}
    }
    Ok(v)
}

/// The deepest nesting of function calls and recursive operations allowed,
/// the default of Nix's `max-call-depth` setting.
pub const MAX_CALL_DEPTH: u32 = 10000;

thread_local! {
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Counts one level of nesting until it is dropped.
pub struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// Enters one level of nesting, failing when the limit is reached.
#[inline]
pub fn depth_guard() -> R<DepthGuard> {
    DEPTH.with(|d| {
        let n = d.get();
        if n >= MAX_CALL_DEPTH {
            return Err(Error::eval("stack overflow; max-call-depth exceeded"));
        }
        d.set(n + 1);
        Ok(DepthGuard)
    })
}
