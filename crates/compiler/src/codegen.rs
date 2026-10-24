//! Translation of Nix expressions into Rust functions.
//!
//! Every expression that must stay unevaluated becomes a *unit*: a Rust
//! function `fn(e: &[Value], a: Value) -> R<Value>` whose `e` holds the values
//! it captured and whose `a` is the argument when the unit is a lambda body.
//! A unit refers to an enclosing variable by capturing it; a unit that needs
//! a variable its parent does not own makes the parent capture it too.
//!
//! Lazy expressions that cannot fail and need no code of their own (literals,
//! variables, lambdas, lists, attribute sets with static names, selections
//! and applications of such) are built directly from runtime thunk kinds, so
//! they cost no generated function.
//!
//! Units are placed into generated crates in the order they are finished; a
//! crate is closed once its source passes a size budget. Units refer to each
//! other only by [`CodeId`](nixpkgs_rust_runtime::value::CodeId), so the
//! crates do not depend on each other.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;

use rnix::ast::{self, AstToken, HasEntry, InterpolPart};
use rowan::ast::AstNode;

use crate::free::{free_vars, ident_text, static_attr_name};
use crate::strings::{Part, rust_bytes, rust_str, string_parts};

pub type Res<T> = Result<T, String>;

/// Attribute names and identifiers, numbered as the runtime numbers them.
pub struct Interner {
    map: HashMap<String, u32>,
    pub names: Vec<String>,
}

impl Interner {
    pub fn new() -> Self {
        let mut i = Interner {
            map: HashMap::new(),
            names: Vec::new(),
        };
        for n in nixpkgs_rust_runtime::symbol::PREDEFINED {
            i.sym(n);
        }
        i
    }

    pub fn sym(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(s.to_string());
        self.map.insert(s.to_string(), id);
        id
    }
}

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

/// The source of one generated crate and the names of its functions, in
/// table order.
#[derive(Default)]
pub struct CrateOut {
    pub src: String,
    pub fns: Vec<String>,
}

struct Binding {
    owner: u32,
    local: String,
}

struct Unit {
    id: u32,
    caps: Vec<u32>,
    cap_idx: HashMap<u32, usize>,
    /// Statics the unit's code refers to, emitted next to its function.
    pre: String,
}

enum Scope {
    Vars(HashMap<String, u32>),
    With(u32),
}

enum Resolved {
    Lexical(u32),
    Global,
    With(Vec<u32>),
}

/// A node of the attribute tree of a set or `let`, after attribute paths
/// have been merged.
#[derive(Clone)]
enum Node {
    Leaf(ast::Expr, u64),
    /// `inherit x;`
    InheritOuter(String, u64),
    /// `inherit (e) x;`, with the index of `e` among the set's temporaries.
    InheritFrom(usize, String, u64),
    Set(Box<SetNode>),
}

#[derive(Clone, Default)]
struct SetNode {
    statics: BTreeMap<String, Node>,
    dynamics: Vec<(ast::Expr, Node, u64)>,
    /// The `e` of each `inherit (e)` at this level.
    temps: Vec<ast::Expr>,
    pos: u64,
    /// Whether the values see the set's own attributes. When attribute paths
    /// merge into a set, its first definition decides this, as in Nix.
    rec: bool,
}

impl Node {
    fn pos(&self) -> u64 {
        match self {
            Node::Leaf(_, p) | Node::InheritOuter(_, p) | Node::InheritFrom(_, _, p) => *p,
            Node::Set(s) => s.pos,
        }
    }
}

impl SetNode {
    fn has_dynamic(&self) -> bool {
        !self.dynamics.is_empty()
            || self.statics.values().any(|n| matches!(n, Node::Set(s) if s.has_dynamic()))
    }
}

fn unparen(e: &ast::Expr) -> ast::Expr {
    let mut e = e.clone();
    while let ast::Expr::Paren(p) = &e {
        match p.expr() {
            Some(x) => e = x,
            None => break,
        }
    }
    e
}

fn need<T>(x: Option<T>, what: &str) -> Res<T> {
    x.ok_or_else(|| format!("syntax error: missing {what}"))
}

/// Translates files into units.
pub struct Compiler {
    pub syms: Interner,
    /// Generated crates; index 0 is a placeholder, crate ids start at 1.
    pub crates: Vec<CrateOut>,
    budget: usize,
    bindings: Vec<Binding>,
    units: Vec<Unit>,
    scopes: Vec<Scope>,
    next_unit: u32,
    next_static: u32,
    file: u32,
}

impl Compiler {
    pub fn new(budget: usize) -> Self {
        Compiler {
            syms: Interner::new(),
            crates: vec![CrateOut::default()],
            budget,
            bindings: Vec::new(),
            units: Vec::new(),
            scopes: Vec::new(),
            next_unit: 0,
            next_static: 0,
            file: 0,
        }
    }

    /// Compiles one file and returns the id of the unit that evaluates it.
    pub fn compile_file(&mut self, file: u32, src: &str) -> Res<u32> {
        self.file = file;
        let parse = rnix::Root::parse(src);
        if let Some(err) = parse.errors().first() {
            return Err(format!("syntax error: {err}"));
        }
        let root = parse.tree();
        let e = need(root.expr(), "expression")?;
        self.units.clear();
        self.scopes.clear();
        self.begin_unit();
        let body = self.expr(&e);
        match body {
            Ok(b) => {
                let u = self.units.pop().unwrap();
                Ok(self.place(u.pre, b))
            }
            Err(err) => {
                self.units.clear();
                self.scopes.clear();
                Err(err)
            }
        }
    }

    fn sym(&mut self, s: &str) -> u32 {
        self.syms.sym(s)
    }

    fn pos_of(&self, n: &rnix::SyntaxNode) -> u64 {
        ((self.file as u64 + 1) << 40) | u64::from(u32::from(n.text_range().start()))
    }

    fn new_binding(&mut self) -> (u32, String) {
        let id = self.bindings.len() as u32;
        let local = format!("v{id}");
        let owner = self.units.last().expect("no unit").id;
        self.bindings.push(Binding {
            owner,
            local: local.clone(),
        });
        (id, local)
    }

    fn begin_unit(&mut self) {
        let id = self.next_unit;
        self.next_unit += 1;
        self.units.push(Unit {
            id,
            caps: Vec::new(),
            cap_idx: HashMap::new(),
            pre: String::new(),
        });
    }

    /// Puts a finished unit into the current crate and returns its code id.
    fn place(&mut self, pre: String, body: String) -> u32 {
        let full = self.crates.len() == 1
            || self.crates.last().unwrap().src.len() > self.budget
            || self.crates.last().unwrap().fns.len() >= (1 << 20) - 1;
        if full {
            self.crates.push(CrateOut::default());
        }
        let ci = self.crates.len() - 1;
        let c = self.crates.last_mut().unwrap();
        let idx = c.fns.len();
        let name = format!("f{idx}");
        c.src.push_str(&pre);
        let _ = writeln!(c.src, "fn {name}(e: &[Value], a: Value) -> R<Value> {{ Ok({body}) }}");
        c.fns.push(name);
        ((ci as u32) << 20) | idx as u32
    }

    /// Ends the innermost unit and returns the code id and the captured
    /// values, as Rust expressions in the enclosing unit.
    fn end_unit(&mut self, body: String) -> (String, Vec<String>) {
        let u = self.units.pop().unwrap();
        let id = self.place(u.pre, body);
        let caps: Vec<String> = u.caps.iter().map(|&b| format!("{}.clone()", self.access(b))).collect();
        (format!("CodeId({id})"), caps)
    }

    fn unit_caps<F>(&mut self, f: F) -> Res<(String, Vec<String>)>
    where
        F: FnOnce(&mut Self) -> Res<String>,
    {
        self.begin_unit();
        let depth = self.scopes.len();
        match f(self) {
            Ok(body) => Ok(self.end_unit(body)),
            Err(e) => {
                self.units.pop();
                self.scopes.truncate(depth);
                Err(e)
            }
        }
    }

    /// Generates a unit and returns its code id and its captured values as
    /// the boxed slice a thunk or lambda takes.
    fn unit<F>(&mut self, f: F) -> Res<(String, String)>
    where
        F: FnOnce(&mut Self) -> Res<String>,
    {
        let (id, caps) = self.unit_caps(f)?;
        Ok((id, format!("Env::from([{}])", caps.join(", "))))
    }

    /// Generates a unit that runs where it is created rather than lazily,
    /// to split the code of one large construct over several functions.
    fn unit_now<F>(&mut self, f: F) -> Res<String>
    where
        F: FnOnce(&mut Self) -> Res<String>,
    {
        let (id, caps) = self.unit_caps(f)?;
        Ok(format!("run({id}, &[{}], Value::Null)?", caps.join(", ")))
    }

    /// The Rust place holding a binding in the innermost unit, capturing it
    /// if the unit does not own it.
    fn access(&mut self, b: u32) -> String {
        let (owner, local) = {
            let bi = &self.bindings[b as usize];
            (bi.owner, bi.local.clone())
        };
        let u = self.units.last_mut().unwrap();
        if u.id == owner {
            return local;
        }
        if let Some(&i) = u.cap_idx.get(&b) {
            return format!("e[{i}]");
        }
        let i = u.caps.len();
        u.caps.push(b);
        u.cap_idx.insert(b, i);
        format!("e[{i}]")
    }

    fn resolve(&self, name: &str, skip: usize) -> Option<Resolved> {
        let scopes = &self.scopes[..self.scopes.len().saturating_sub(skip)];
        for s in scopes.iter().rev() {
            if let Scope::Vars(m) = s
                && let Some(&b) = m.get(name)
            {
                return Some(Resolved::Lexical(b));
            }
        }
        if nixpkgs_rust_runtime::builtins::is_global(name) {
            return Some(Resolved::Global);
        }
        let withs: Vec<u32> = scopes
            .iter()
            .rev()
            .filter_map(|s| match s {
                Scope::With(b) => Some(*b),
                _ => None,
            })
            .collect();
        if withs.is_empty() { None } else { Some(Resolved::With(withs)) }
    }

    /// A variable reference, as a value expression.
    fn var(&mut self, name: &str, skip: usize) -> Res<String> {
        match self.resolve(name, skip) {
            Some(Resolved::Lexical(b)) => Ok(format!("{}.clone()", self.access(b))),
            Some(Resolved::Global) => Ok(match name {
                "true" => "Value::Bool(true)".to_string(),
                "false" => "Value::Bool(false)".to_string(),
                "null" => "Value::Null".to_string(),
                "builtins" => "builtins()".to_string(),
                _ => format!("global(Sym({}))?", self.sym(name)),
            }),
            Some(Resolved::With(ws)) => {
                let refs: Vec<String> = ws.iter().map(|&b| format!("&{}", self.access(b))).collect();
                Ok(format!("with_lookup(&[{}], Sym({}))?", refs.join(", "), self.sym(name)))
            }
            None => Err(format!("undefined variable '{name}'")),
        }
    }

    /// A variable reference that stays unevaluated.
    fn lazy_var(&mut self, name: &str, skip: usize) -> Res<String> {
        match self.resolve(name, skip) {
            Some(Resolved::With(_)) => {
                let n = name.to_string();
                let (id, env) = self.unit(move |c| c.var(&n, skip))?;
                Ok(format!("Value::thunk({id}, {env})"))
            }
            _ => self.var(name, skip),
        }
    }

    // -----------------------------------------------------------------------
    // Laziness

    fn is_simple(&self, e: &ast::Expr) -> bool {
        use ast::Expr::*;
        match e {
            Paren(p) => p.expr().is_some_and(|x| self.is_simple(&x)),
            Literal(_) | CurPos(_) | Lambda(_) | List(_) => true,
            Str(s) => s.parts().all(|p| matches!(p, InterpolPart::Literal(_))),
            PathAbs(p) => p.parts().len() <= 1,
            PathRel(p) => p.parts().len() <= 1,
            PathHome(p) => p.parts().len() <= 1,
            Ident(i) => matches!(
                self.resolve(&ident_text(i), 0),
                Some(Resolved::Lexical(_) | Resolved::Global)
            ),
            Select(s) => {
                s.default_expr().is_none()
                    && s.attrpath().is_some_and(|p| p.attrs().all(|a| static_attr_name(&a).is_some()))
                    && s.expr().is_some_and(|x| self.is_simple(&x))
            }
            Apply(a) => {
                a.lambda().is_some_and(|x| self.is_simple(&x)) && a.argument().is_some_and(|x| self.is_simple(&x))
            }
            AttrSet(s) => !entries_dynamic(s),
            _ => false,
        }
    }

    /// Builds a simple expression without evaluating it.
    fn simple(&mut self, e: &ast::Expr) -> Res<String> {
        use ast::Expr::*;
        match e {
            Paren(p) => self.simple(&need(p.expr(), "expression")?),
            Select(s) => {
                let mut code = self.simple(&need(s.expr(), "expression")?)?;
                for a in need(s.attrpath(), "attribute path")?.attrs() {
                    let n = static_attr_name(&a).unwrap();
                    code = format!("Value::sel({code}, Sym({}))", self.sym(&n));
                }
                Ok(code)
            }
            Apply(a) => {
                let f = self.simple(&need(a.lambda(), "function")?)?;
                let x = self.simple(&need(a.argument(), "argument")?)?;
                Ok(format!("Value::app({f}, {x})"))
            }
            _ => self.expr(e),
        }
    }

    /// An expression that stays unevaluated until it is forced.
    fn lazy(&mut self, e: &ast::Expr) -> Res<String> {
        if self.is_simple(e) {
            return self.simple(e);
        }
        let e = e.clone();
        let (id, env) = self.unit(move |c| c.expr(&e))?;
        Ok(format!("Value::thunk({id}, {env})"))
    }

    // -----------------------------------------------------------------------
    // Expressions

    /// An expression evaluated where it appears. The resulting value may
    /// still be a thunk; whatever consumes it forces it.
    fn expr(&mut self, e: &ast::Expr) -> Res<String> {
        use ast::Expr::*;
        Ok(match e {
            Paren(p) => self.expr(&need(p.expr(), "expression")?)?,
            Root(r) => self.expr(&need(r.expr(), "expression")?)?,
            Literal(l) => self.literal(l)?,
            Str(s) => self.string(s)?,
            Ident(i) => self.var(&ident_text(i), 0)?,
            PathAbs(_) | PathRel(_) | PathHome(_) | PathSearch(_) => self.path(e)?,
            Lambda(l) => self.lambda(l)?,
            Apply(a) => {
                let f = self.operand(&need(a.lambda(), "function")?)?;
                let x = self.lazy(&need(a.argument(), "argument")?)?;
                format!("call(&({f}), {x})?")
            }
            Select(s) => self.select(s)?,
            HasAttr(h) => {
                let base = self.operand(&need(h.expr(), "expression")?)?;
                let path = self.attr_path(&need(h.attrpath(), "attribute path")?)?;
                format!("Value::Bool(has_attr_path(&({base}), &{path})?)")
            }
            IfElse(i) => {
                let c = self.operand(&need(i.condition(), "condition")?)?;
                let t = self.expr(&need(i.body(), "then branch")?)?;
                let f = self.expr(&need(i.else_body(), "else branch")?)?;
                format!("if truthy(&({c}))? {{ {t} }} else {{ {f} }}")
            }
            Assert(a) => {
                let cond_e = need(a.condition(), "condition")?;
                let c = self.operand(&cond_e)?;
                let text = cond_e.syntax().text().to_string();
                let b = self.expr(&need(a.body(), "body")?)?;
                format!(
                    "{{ if !truthy(&({c}))? {{ return Err(assertion_failed({})); }} {b} }}",
                    rust_str(&text)
                )
            }
            LetIn(l) => {
                let set = self.build_set(l, false)?;
                let (decls, _) = self.group(set)?;
                let body = self.expr(&need(l.body(), "body")?);
                self.scopes.pop();
                format!("{{ {decls} {} }}", body?)
            }
            LegacyLet(l) => {
                let set = self.build_set(l, true)?;
                let attrs = self.rec_set(set)?;
                format!("select(&({attrs}), Sym({}))?", self.sym("body"))
            }
            With(w) => {
                let ns = self.lazy(&need(w.namespace(), "namespace")?)?;
                let (b, local) = self.new_binding();
                self.scopes.push(Scope::With(b));
                let body = self.expr(&need(w.body(), "body")?);
                self.scopes.pop();
                format!("{{ let {local} = {ns}; {} }}", body?)
            }
            List(l) => {
                let items: Vec<ast::Expr> = l.items().collect();
                self.list_items(&items)?
            }
            AttrSet(s) => {
                if s.rec_token().is_some() {
                    let set = self.build_set(s, true)?;
                    self.rec_set(set)?
                } else {
                    let set = self.build_set(s, true)?;
                    self.set_expr(&set)?
                }
            }
            BinOp(b) => self.binop(b)?,
            UnaryOp(u) => {
                let x = self.operand(&need(u.expr(), "operand")?)?;
                match need(u.operator(), "operator")? {
                    ast::UnaryOpKind::Invert => format!("not(&({x}))?"),
                    ast::UnaryOpKind::Negate => format!("neg(&({x}))?"),
                }
            }
            CurPos(c) => format!("pos_value(Pos({}))", self.pos_of(c.syntax())),
            Error(_) => return Err("syntax error".to_string()),
        })
    }

    /// An expression whose value is only borrowed where it appears: a
    /// variable bound by a `let`, a function argument or a recursive set is
    /// used in place rather than cloned; anything else is evaluated as by
    /// [`Compiler::expr`].
    fn operand(&mut self, e: &ast::Expr) -> Res<String> {
        if let ast::Expr::Ident(i) = unparen(e)
            && let Some(Resolved::Lexical(b)) = self.resolve(&ident_text(&i), 0)
        {
            return Ok(self.access(b));
        }
        self.expr(e)
    }

    fn literal(&mut self, l: &ast::Literal) -> Res<String> {
        Ok(match l.kind() {
            ast::LiteralKind::Integer(i) => {
                let v = i
                    .value()
                    .map_err(|_| format!("invalid integer '{}'", i.syntax().text()))?;
                format!("Value::Int({v})")
            }
            ast::LiteralKind::Float(f) => {
                let v = f
                    .value()
                    .map_err(|_| format!("invalid float '{}'", f.syntax().text()))?;
                format!("Value::Float(f64::from_bits({:#x}))", v.to_bits())
            }
            ast::LiteralKind::Uri(u) => format!("Value::lit({})", rust_bytes(u.syntax().text().as_bytes())),
        })
    }

    fn string(&mut self, s: &ast::Str) -> Res<String> {
        let parts = string_parts(s);
        if parts.iter().any(|p| matches!(p, Part::Lit(b) if b.contains(&0))) {
            return Err("input string contains NUL byte".to_string());
        }
        match parts.as_slice() {
            [] => return Ok("Value::lit(b\"\")".to_string()),
            [Part::Lit(b)] => return Ok(format!("Value::lit({})", rust_bytes(b))),
            _ => {}
        }
        let mut items = Vec::new();
        for p in &parts {
            items.push(match p {
                Part::Lit(b) => format!("Value::lit({})", rust_bytes(b)),
                Part::Interp(i) => self.expr(&need(i.expr(), "interpolated expression")?)?,
            });
        }
        Ok(format!("concat(&[{}], true)?", items.join(", ")))
    }

    fn path(&mut self, e: &ast::Expr) -> Res<String> {
        use ast::Expr::*;
        if let PathSearch(p) = e {
            let text = need(p.content(), "path")?.syntax().text().to_string();
            let name = text.trim_start_matches('<').trim_end_matches('>').to_string();
            let ff = self.var("__findFile", 0)?;
            let np = self.lazy_var("__nixPath", 0)?;
            return Ok(format!(
                "call(&call(&({ff}), {np})?, Value::lit({}))?",
                rust_bytes(name.as_bytes())
            ));
        }
        let (parts, kind) = match e {
            PathAbs(p) => (p.parts(), 'a'),
            PathRel(p) => (p.parts(), 'r'),
            PathHome(p) => (p.parts(), 'h'),
            _ => unreachable!(),
        };
        let file = self.file;
        let prefix = |text: &str, whole: bool| -> String {
            match (kind, whole) {
                ('a', true) => format!(
                    "Value::Path(std::rc::Rc::from({}))",
                    rust_str(&nixpkgs_rust_runtime::paths::canon_path(text))
                ),
                ('a', false) => format!("Value::Path(std::rc::Rc::from({}))", rust_str(text)),
                ('r', true) => format!("path_lit({file}, {})", rust_str(text)),
                ('r', false) => format!("path_prefix({file}, {})", rust_str(text)),
                (_, true) => format!(
                    "nixpkgs_rust_runtime::program::home_path({}, false)",
                    rust_str(text.trim_start_matches('~'))
                ),
                (_, false) => format!(
                    "nixpkgs_rust_runtime::program::home_path({}, true)",
                    rust_str(text.trim_start_matches('~'))
                ),
            }
        };
        if let [InterpolPart::Literal(t)] = parts.as_slice() {
            return Ok(prefix(t.syntax().text(), true));
        }
        let mut items = Vec::new();
        for (n, p) in parts.iter().enumerate() {
            items.push(match p {
                InterpolPart::Literal(t) if n == 0 => prefix(t.syntax().text(), false),
                InterpolPart::Literal(t) => format!("Value::lit({})", rust_bytes(t.syntax().text().as_bytes())),
                InterpolPart::Interpolation(i) => self.expr(&need(i.expr(), "interpolated expression")?)?,
            });
        }
        Ok(format!("concat(&[{}], false)?", items.join(", ")))
    }

    /// An attribute path as a Rust array of symbols; dynamic names are
    /// evaluated.
    fn attr_path(&mut self, p: &ast::Attrpath) -> Res<String> {
        let mut items = Vec::new();
        for a in p.attrs() {
            items.push(match static_attr_name(&a) {
                Some(n) => format!("Sym({})", self.sym(&n)),
                None => {
                    let e = dynamic_key(&a)?;
                    format!("dyn_sel_name(&({}))?", self.expr(&e)?)
                }
            });
        }
        Ok(format!("[{}]", items.join(", ")))
    }

    fn select(&mut self, s: &ast::Select) -> Res<String> {
        let base = self.operand(&need(s.expr(), "expression")?)?;
        let p = need(s.attrpath(), "attribute path")?;
        let attrs: Vec<ast::Attr> = p.attrs().collect();
        match s.default_expr() {
            None => {
                if attrs.len() == 1
                    && let Some(n) = static_attr_name(&attrs[0])
                {
                    return Ok(format!("select(&({base}), Sym({}))?", self.sym(&n)));
                }
                let path = self.attr_path(&p)?;
                Ok(format!("select_path(&({base}), &{path})?"))
            }
            Some(d) => {
                let path = self.attr_path(&p)?;
                let d = self.expr(&d)?;
                Ok(format!("match select_or(&({base}), &{path})? {{ Some(v) => v, None => {d} }}"))
            }
        }
    }

    fn binop(&mut self, b: &ast::BinOp) -> Res<String> {
        use ast::BinOpKind::*;
        let lhs = need(b.lhs(), "left operand")?;
        let rhs = need(b.rhs(), "right operand")?;
        let op = need(b.operator(), "operator")?;
        if op == PipeRight {
            let x = self.lazy(&lhs)?;
            let f = self.operand(&rhs)?;
            return Ok(format!("call(&({f}), {x})?"));
        }
        if op == PipeLeft {
            let f = self.operand(&lhs)?;
            let x = self.lazy(&rhs)?;
            return Ok(format!("call(&({f}), {x})?"));
        }
        let l = self.operand(&lhs)?;
        let r = self.operand(&rhs)?;
        Ok(match op {
            Concat => format!("concat_lists(&({l}), &({r}))?"),
            Update => format!("update(&({l}), &({r}))?"),
            Add => format!("add(&({l}), &({r}))?"),
            Sub => format!("sub(&({l}), &({r}))?"),
            Mul => format!("mul(&({l}), &({r}))?"),
            Div => format!("div(&({l}), &({r}))?"),
            And => format!("Value::Bool(truthy(&({l}))? && truthy(&({r}))?)"),
            Or => format!("Value::Bool(truthy(&({l}))? || truthy(&({r}))?)"),
            Implication => format!("Value::Bool(!truthy(&({l}))? || truthy(&({r}))?)"),
            Equal => format!("Value::Bool(eq(&({l}), &({r}))?)"),
            NotEqual => format!("Value::Bool(!eq(&({l}), &({r}))?)"),
            Less => format!("Value::Bool(less_than(&({l}), &({r}))?)"),
            // The right operand is evaluated first, as Nix evaluates `a > b`
            // as `b < a`.
            More => format!("{{ let r = &({r}); Value::Bool(less_than(r, &({l}))?) }}"),
            LessOrEq => format!("{{ let r = &({r}); Value::Bool(!less_than(r, &({l}))?) }}"),
            MoreOrEq => format!("Value::Bool(!less_than(&({l}), &({r}))?)"),
            PipeRight | PipeLeft => unreachable!(),
        })
    }

    fn lambda(&mut self, l: &ast::Lambda) -> Res<String> {
        let pos = self.pos_of(l.syntax());
        let param = need(l.param(), "parameter")?;
        let body_e = need(l.body(), "body")?;
        let info_name = format!("L{}", self.next_static);
        self.next_static += 1;

        // The static description of the lambda.
        let mut info = String::new();
        let mut formal_list: Vec<(String, Option<ast::Expr>, u64)> = Vec::new();
        let mut at_name = None;
        let mut ellipsis = false;
        match &param {
            ast::Param::IdentParam(p) => {
                let n = ident_text(&need(p.ident(), "parameter name")?);
                let s = self.sym(&n);
                let _ = write!(
                    info,
                    "static {info_name}: LambdaInfo = LambdaInfo {{ arg: Some(Sym({s})), formals: None, pos: Pos({pos}) }};\n"
                );
            }
            ast::Param::Pattern(p) => {
                ellipsis = p.ellipsis_token().is_some();
                at_name = p.pat_bind().and_then(|b| b.ident()).map(|i| ident_text(&i));
                let mut seen = HashSet::new();
                for e in p.pat_entries() {
                    let n = ident_text(&need(e.ident(), "formal name")?);
                    if !seen.insert(n.clone()) {
                        return Err(format!("duplicate formal function argument '{n}'"));
                    }
                    let fpos = self.pos_of(e.syntax());
                    formal_list.push((n, e.default(), fpos));
                }
                if let Some(a) = &at_name
                    && seen.contains(a)
                {
                    return Err(format!("duplicate formal function argument '{a}'"));
                }
                let mut sorted: Vec<(u32, bool, u64)> = formal_list
                    .iter()
                    .map(|(n, d, p)| (self.syms.sym(n), d.is_some(), *p))
                    .collect();
                sorted.sort();
                let names: Vec<String> = sorted
                    .iter()
                    .map(|(s, d, p)| format!("(Sym({s}), {d}, Pos({p}))"))
                    .collect();
                let arg = match &at_name {
                    Some(a) => format!("Some(Sym({}))", self.sym(a)),
                    None => "None".to_string(),
                };
                let _ = write!(
                    info,
                    "static {info_name}: LambdaInfo = LambdaInfo {{ arg: {arg}, formals: Some(Formals {{ names: &[{}], ellipsis: {ellipsis} }}), pos: Pos({pos}) }};\n",
                    names.join(", ")
                );
            }
        }

        let (id, env) = self.unit(|c| {
            let mut stmts = String::new();
            let mut scope = HashMap::new();
            match &param {
                ast::Param::IdentParam(p) => {
                    let n = ident_text(&need(p.ident(), "parameter name")?);
                    let (b, local) = c.new_binding();
                    let _ = write!(stmts, "let {local} = a; ");
                    scope.insert(n, b);
                    c.scopes.push(Scope::Vars(scope));
                }
                ast::Param::Pattern(_) => {
                    let _ = write!(stmts, "let args = force_attrs(&a)?; ");
                    if !ellipsis {
                        let _ = write!(stmts, "check_formals(&args, &{info_name})?; ");
                    }
                    let mut locals = Vec::new();
                    for (n, _, _) in &formal_list {
                        let (b, local) = c.new_binding();
                        scope.insert(n.clone(), b);
                        locals.push(local);
                    }
                    if let Some(a) = &at_name {
                        let (b, local) = c.new_binding();
                        scope.insert(a.clone(), b);
                        let _ = write!(stmts, "let {local} = a.clone(); ");
                    }
                    let names: HashSet<String> = scope.keys().cloned().collect();
                    let recursive = formal_list
                        .iter()
                        .filter_map(|(_, d, _)| d.as_ref())
                        .any(|d| free_vars(d).iter().any(|v| names.contains(v)));
                    c.scopes.push(Scope::Vars(scope));
                    let mut inits = String::new();
                    for ((n, default, _), local) in formal_list.iter().zip(&locals) {
                        let s = c.sym(n);
                        match default {
                            None => {
                                let _ = write!(
                                    stmts,
                                    "let {local} = match args.get(Sym({s})) {{ Some(v) => v.clone(), None => return Err(formal_missing(Sym({s}), &{info_name})) }}; "
                                );
                            }
                            Some(d) if !recursive => {
                                let dv = c.lazy(d)?;
                                let _ = write!(
                                    stmts,
                                    "let {local} = match args.get(Sym({s})) {{ Some(v) => v.clone(), None => {dv} }}; "
                                );
                            }
                            Some(d) => {
                                let _ = write!(
                                    stmts,
                                    "let {local} = match args.get(Sym({s})) {{ Some(v) => v.clone(), None => Value::blank() }}; "
                                );
                                let init = c.init_code(local, d)?;
                                let _ = write!(inits, "if !args.contains(Sym({s})) {{ {init} }} ");
                            }
                        }
                    }
                    stmts.push_str(&inits);
                }
            }
            let body = c.expr(&body_e);
            c.scopes.pop();
            // The body checks its arguments against the description.
            c.units.last_mut().unwrap().pre.push_str(&info);
            Ok(format!("{{ {stmts}{} }}", body?))
        })?;
        // The creating unit may land in another crate than the body, and
        // generated crates do not refer to each other, so it gets its own
        // copy of the description under another name.
        let creator_name = format!("M{}", &info_name[1..]);
        let creator_info = info.replacen(&format!("static {info_name}:"), &format!("static {creator_name}:"), 1);
        self.units.last_mut().unwrap().pre.push_str(&creator_info);
        Ok(format!("Value::lambda({id}, {env}, &{creator_name})"))
    }

    /// Fills a blank thunk with an expression.
    fn init_code(&mut self, local: &str, e: &ast::Expr) -> Res<String> {
        if self.is_simple(e) {
            let v = self.simple(e)?;
            return Ok(format!("{local}.init_value({v});"));
        }
        let e = e.clone();
        let (id, env) = self.unit(move |c| c.expr(&e))?;
        Ok(format!("{local}.init({id}, {env});"))
    }

    // -----------------------------------------------------------------------
    // Attribute sets and `let`

    fn build_set(&mut self, h: &impl HasEntry, allow_dynamic: bool) -> Res<SetNode> {
        let mut set = SetNode::default();
        for entry in h.entries() {
            match entry {
                ast::Entry::AttrpathValue(av) => {
                    let p = need(av.attrpath(), "attribute path")?;
                    let pos = self.pos_of(p.syntax());
                    let attrs: Vec<ast::Attr> = p.attrs().collect();
                    let value = need(av.value(), "value")?;
                    self.insert_path(&mut set, &attrs, Node::Leaf(value, pos), pos, allow_dynamic)?;
                }
                ast::Entry::Inherit(inh) => {
                    let temp = match inh.from().and_then(|f| f.expr()) {
                        Some(e) => {
                            set.temps.push(e);
                            Some(set.temps.len() - 1)
                        }
                        None => None,
                    };
                    for a in inh.attrs() {
                        let n = static_attr_name(&a)
                            .ok_or_else(|| "dynamic attributes not allowed in inherit".to_string())?;
                        let pos = self.pos_of(a.syntax());
                        let node = match temp {
                            Some(t) => Node::InheritFrom(t, n.clone(), pos),
                            None => Node::InheritOuter(n.clone(), pos),
                        };
                        if set.statics.contains_key(&n) {
                            return Err(format!("attribute '{n}' already defined"));
                        }
                        set.statics.insert(n, node);
                    }
                }
            }
        }
        Ok(set)
    }

    /// Turns a node into a set node when it is a literal set.
    fn to_set(&mut self, node: Node, name: &str) -> Res<SetNode> {
        match node {
            Node::Set(s) => Ok(*s),
            Node::Leaf(e, pos) => match unparen(&e) {
                ast::Expr::AttrSet(a) => {
                    let mut s = self.build_set(&a, true)?;
                    s.pos = pos;
                    s.rec = a.rec_token().is_some();
                    Ok(s)
                }
                _ => Err(format!("attribute '{name}' already defined")),
            },
            _ => Err(format!("attribute '{name}' already defined")),
        }
    }

    fn merge(&mut self, into: &mut SetNode, from: SetNode) -> Res<()> {
        let offset = into.temps.len();
        into.temps.extend(from.temps);
        for (k, n) in from.statics {
            let n = shift_temps(n, offset);
            match into.statics.remove(&k) {
                None => {
                    into.statics.insert(k, n);
                }
                Some(existing) => {
                    let mut a = self.to_set(existing, &k)?;
                    let b = self.to_set(n, &k)?;
                    self.merge(&mut a, b)?;
                    into.statics.insert(k, Node::Set(Box::new(a)));
                }
            }
        }
        for (k, n, p) in from.dynamics {
            into.dynamics.push((k, shift_temps(n, offset), p));
        }
        Ok(())
    }

    fn insert_path(
        &mut self,
        set: &mut SetNode,
        attrs: &[ast::Attr],
        leaf: Node,
        pos: u64,
        allow_dynamic: bool,
    ) -> Res<()> {
        let first = &attrs[0];
        match static_attr_name(first) {
            Some(k) => {
                if attrs.len() == 1 {
                    match set.statics.remove(&k) {
                        None => {
                            set.statics.insert(k, leaf);
                        }
                        Some(existing) => {
                            let mut a = self.to_set(existing, &k)?;
                            let b = self.to_set(leaf, &k)?;
                            self.merge(&mut a, b)?;
                            set.statics.insert(k, Node::Set(Box::new(a)));
                        }
                    }
                } else {
                    let existing = set.statics.remove(&k);
                    let mut sub = match existing {
                        None => SetNode {
                            pos,
                            ..Default::default()
                        },
                        Some(n) => self.to_set(n, &k)?,
                    };
                    self.insert_path(&mut sub, &attrs[1..], leaf, pos, true)?;
                    set.statics.insert(k, Node::Set(Box::new(sub)));
                }
            }
            None => {
                if !allow_dynamic {
                    return Err("dynamic attributes not allowed in let".to_string());
                }
                let key = dynamic_key(first)?;
                if attrs.len() == 1 {
                    set.dynamics.push((key, leaf, pos));
                } else {
                    let mut sub = SetNode {
                        pos,
                        ..Default::default()
                    };
                    self.insert_path(&mut sub, &attrs[1..], leaf, pos, true)?;
                    set.dynamics.push((key, Node::Set(Box::new(sub)), pos));
                }
            }
        }
        Ok(())
    }

    /// The value of a node in a non-recursive set, unevaluated. `temps` are
    /// the bindings holding the `e` of each `inherit (e)` of the set.
    fn node_value(&mut self, node: &Node, temps: &[u32]) -> Res<String> {
        Ok(match node {
            Node::Leaf(e, _) => self.lazy(e)?,
            Node::InheritOuter(n, _) => self.lazy_var(n, 0)?,
            Node::InheritFrom(t, n, _) => {
                let place = self.access(temps[*t]);
                format!("Value::sel({place}.clone(), Sym({}))", self.sym(n))
            }
            Node::Set(s) => self.set_value(s)?,
        })
    }

    /// Static attributes, sorted by symbol, as a set value: built in one
    /// function, or in several when the source is large.
    fn static_entries(&mut self, entries: &[(u32, &Node)], temps: &[u32]) -> Res<String> {
        let total: usize = entries.iter().map(|e| node_src_len(e.1)).sum();
        // A single entry is not split; its value is split where it is built.
        if total <= 2 * CHUNK || entries.len() <= 1 {
            let mut list = Vec::with_capacity(entries.len());
            for (s, node) in entries {
                let v = self.node_value(node, temps)?;
                list.push(format!("Attr {{ k: Sym({s}), v: {v}, pos: Pos({}) }}", node.pos()));
            }
            return Ok(format!("attrs_static(vec![{}])", list.join(", ")));
        }
        let mut parts = Vec::new();
        for chunk in chunks_by(entries, |e| node_src_len(e.1)) {
            parts.push(self.unit_now(|c| c.static_entries(chunk, temps))?);
        }
        Ok(format!("concat_attrs(vec![{}])?", parts.join(", ")))
    }

    /// The elements of a list, built in one function or, when the source is
    /// large, in several.
    fn list_items(&mut self, items: &[ast::Expr]) -> Res<String> {
        let len = |e: &ast::Expr| u32::from(e.syntax().text_range().len()) as usize;
        let total: usize = items.iter().map(len).sum();
        if total <= 2 * CHUNK || items.len() <= 1 {
            let mut out = Vec::with_capacity(items.len());
            for x in items {
                out.push(self.lazy(x)?);
            }
            return Ok(format!("Value::list(vec![{}])", out.join(", ")));
        }
        let mut parts = Vec::new();
        for chunk in chunks_by(items, len) {
            parts.push(self.unit_now(|c| c.list_items(chunk))?);
        }
        Ok(format!("concat_list_parts(vec![{}])?", parts.join(", ")))
    }

    /// A nested set as an attribute value: built in place when it has only
    /// static names, otherwise as a thunk (its dynamic names are evaluated
    /// when it is).
    fn set_value(&mut self, s: &SetNode) -> Res<String> {
        if !s.has_dynamic() {
            return self.any_set(s.clone());
        }
        let s = s.clone();
        let (id, env) = self.unit(move |c| c.any_set(s))?;
        Ok(format!("Value::thunk({id}, {env})"))
    }

    /// A set node, recursive or not.
    fn any_set(&mut self, s: SetNode) -> Res<String> {
        if s.rec { self.rec_set(s) } else { self.set_expr(&s) }
    }

    /// A non-recursive set.
    fn set_expr(&mut self, set: &SetNode) -> Res<String> {
        let mut pre = String::new();
        let mut temps = Vec::new();
        for t in &set.temps {
            let code = self.lazy(t)?;
            let (b, local) = self.new_binding();
            let _ = write!(pre, "let {local} = {code}; ");
            temps.push(b);
        }
        let mut entries: Vec<(u32, &Node)> = set.statics.iter().map(|(k, n)| (self.syms.sym(k), n)).collect();
        entries.sort_by_key(|e| e.0);
        let core = self.static_entries(&entries, &temps)?;
        if set.dynamics.is_empty() {
            return Ok(if pre.is_empty() { core } else { format!("{{ {pre}{core} }}") });
        }
        let mut code = format!("{{ {pre}let mut a_ = (*force_attrs(&({core}))?).clone(); ");
        for (k, node, p) in &set.dynamics {
            let key = self.expr(k)?;
            let v = self.node_value(node, &temps)?;
            let _ = write!(code, "insert_dynamic(&mut a_, &({key}), {v}, Pos({p}))?; ");
        }
        code.push_str("Value::attrs(a_) }");
        Ok(code)
    }

    /// The bindings of a `let` or recursive set. Pushes the scope of the
    /// group, which the caller pops, and returns the declarations together
    /// with the static attributes as (name, binding, position).
    fn group(&mut self, set: SetNode) -> Res<(String, Vec<(String, u32, u64)>)> {
        self.group_with(set, false)
    }

    /// [`Compiler::group`]. With `proxies`, used for a recursive set with
    /// `__overrides`, each name other than `__overrides` refers to a proxy
    /// thunk that takes the value from `__overrides` when it has the name,
    /// so that the set's own attributes see the overridden values.
    fn group_with(&mut self, mut set: SetNode, proxies: bool) -> Res<(String, Vec<(String, u32, u64)>)> {
        struct Item {
            name: Option<String>,
            node: Node,
            binding: u32,
            local: String,
        }
        let temps = std::mem::take(&mut set.temps);
        let statics = std::mem::take(&mut set.statics);
        let mut items: Vec<Item> = Vec::new();
        for t in temps {
            let (binding, local) = self.new_binding();
            let pos = 0;
            items.push(Item {
                name: None,
                node: Node::Leaf(t, pos),
                binding,
                local,
            });
        }
        let ntemps = items.len();
        let mut scope = HashMap::new();
        // (proxy local, item index, name) for each proxied name.
        let mut proxy_list: Vec<(String, u32, usize, String)> = Vec::new();
        for (k, node) in statics {
            let (binding, local) = self.new_binding();
            if proxies && k != "__overrides" {
                let (pb, pl) = self.new_binding();
                scope.insert(k.clone(), pb);
                proxy_list.push((pl, pb, items.len(), k.clone()));
            } else {
                scope.insert(k.clone(), binding);
            }
            items.push(Item {
                name: Some(k),
                node,
                binding,
                local,
            });
        }
        let names: HashSet<String> = scope.keys().cloned().collect();
        let index: HashMap<String, usize> = items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| it.name.clone().map(|n| (n, i)))
            .collect();
        // What each item refers to within the group.
        let refs: Vec<HashSet<usize>> = items
            .iter()
            .map(|it| {
                let mut r = HashSet::new();
                match &it.node {
                    Node::Leaf(e, _) => {
                        for v in free_vars(e) {
                            if let Some(&i) = index.get(&v) {
                                r.insert(i);
                            }
                        }
                    }
                    Node::InheritOuter(..) => {}
                    Node::InheritFrom(t, _, _) => {
                        r.insert(*t);
                    }
                    Node::Set(s) => {
                        for v in set_free_vars(s) {
                            if names.contains(&v)
                                && let Some(&i) = index.get(&v)
                            {
                                r.insert(i);
                            }
                        }
                    }
                }
                r
            })
            .collect();
        self.scopes.push(Scope::Vars(scope));

        // Items whose references are all built before them are built
        // directly, in dependency order; the rest start as blank thunks.
        let mut direct: Vec<bool> = vec![false; items.len()];
        let mut order = Vec::new();
        while !proxies {
            let mut progress = false;
            for i in 0..items.len() {
                if !direct[i] && refs[i].iter().all(|&j| j != i && direct[j]) {
                    direct[i] = true;
                    order.push(i);
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
        let temp_bindings: Vec<u32> = items[..ntemps].iter().map(|it| it.binding).collect();
        let mut code = String::new();
        for (pl, _, _, _) in &proxy_list {
            let _ = write!(code, "let {pl} = Value::blank(); ");
        }
        // A large group keeps its thunks in one vector instead of a local
        // each, and fills them from several functions.
        let total: usize = items.iter().map(|it| node_src_len(&it.node)).sum();
        let big = !proxies && (total > 2 * CHUNK || items.len() > 512);
        if big {
            direct.iter_mut().for_each(|d| *d = false);
            order.clear();
            let g = format!("g{}", self.next_static);
            self.next_static += 1;
            let _ = write!(code, "let {g} = Value::blanks({}); ", items.len());
            for (i, it) in items.iter_mut().enumerate() {
                let place = format!("{g}[{i}]");
                self.bindings[it.binding as usize].local = place.clone();
                it.local = place;
            }
        }
        for &i in &order {
            let v = match &items[i].node {
                Node::InheritOuter(n, _) => self.lazy_var(n, 1)?,
                node => self.node_value(node, &temp_bindings)?,
            };
            let _ = write!(code, "let {} = {v}; ", items[i].local);
        }
        if !big {
            for (i, it) in items.iter().enumerate() {
                if !direct[i] {
                    let _ = write!(code, "let {} = Value::blank(); ", it.local);
                }
            }
        }
        let pending: Vec<(u32, Node)> = items
            .iter()
            .enumerate()
            .filter(|(i, _)| !direct[*i])
            .map(|(_, it)| (it.binding, it.node.clone()))
            .collect();
        if big {
            for chunk in chunks_by(&pending, |p| node_src_len(&p.1)) {
                let call = self.unit_now(|c| {
                    let mut body = String::from("{ ");
                    for (b, node) in chunk {
                        body.push_str(&c.item_init(*b, node, &temp_bindings)?);
                        body.push(' ');
                    }
                    body.push_str("Value::Null }");
                    Ok(body)
                })?;
                let _ = write!(code, "{call}; ");
            }
        } else {
            for (b, node) in &pending {
                code.push_str(&self.item_init(*b, node, &temp_bindings)?);
                code.push(' ');
            }
        }
        let mut attrs: Vec<(String, u32, u64)> = items
            .iter()
            .filter_map(|it| it.name.clone().map(|n| (n, it.binding, it.node.pos())))
            .collect();
        if proxies {
            let ov = items
                .iter()
                .find(|it| it.name.as_deref() == Some("__overrides"))
                .map(|it| it.local.clone())
                .expect("proxies need __overrides");
            for (pl, pb, idx, n) in &proxy_list {
                let s = self.sym(n);
                let own = items[*idx].local.clone();
                let _ = write!(code, "{pl}.init_value(override_or({ov}.clone(), Sym({s}), {own}.clone())); ");
                for a in attrs.iter_mut() {
                    if &a.0 == n {
                        a.1 = *pb;
                    }
                }
            }
        }
        set.temps = Vec::new();
        Ok((code, attrs))
    }

    /// Fills the blank thunk of one binding of a group.
    fn item_init(&mut self, b: u32, node: &Node, temps: &[u32]) -> Res<String> {
        let place = self.access(b);
        Ok(match node {
            Node::Leaf(e, _) => self.init_code(&place, e)?,
            Node::InheritOuter(n, _) => format!("{place}.init_value({});", self.lazy_var(n, 1)?),
            Node::InheritFrom(t, n, _) => {
                let tp = self.access(temps[*t]);
                format!("{place}.init_value(Value::sel({tp}.clone(), Sym({})));", self.sym(n))
            }
            Node::Set(s) => {
                if s.has_dynamic() {
                    let s = (**s).clone();
                    let (id, env) = self.unit(move |c| c.any_set(s))?;
                    format!("{place}.init({id}, {env});")
                } else {
                    format!("{place}.init_value({});", self.any_set((**s).clone())?)
                }
            }
        })
    }

    /// The attributes of a recursive set, (symbol, binding, position) sorted
    /// by symbol, as a set value; in several functions when there are many.
    fn rec_entries(&mut self, entries: &[(u32, u32, u64)]) -> Res<String> {
        if entries.len() <= 512 {
            let mut list = Vec::with_capacity(entries.len());
            for (s, b, p) in entries {
                let place = self.access(*b);
                list.push(format!("Attr {{ k: Sym({s}), v: {place}.clone(), pos: Pos({p}) }}"));
            }
            return Ok(format!("attrs_static(vec![{}])", list.join(", ")));
        }
        let mut parts = Vec::new();
        for chunk in entries.chunks(512) {
            parts.push(self.unit_now(|c| c.rec_entries(chunk))?);
        }
        Ok(format!("concat_attrs(vec![{}])?", parts.join(", ")))
    }

    /// A recursive set. A static `__overrides` attribute, a legacy feature,
    /// replaces attributes of the result with its own.
    fn rec_set(&mut self, set: SetNode) -> Res<String> {
        let overrides = set.statics.contains_key("__overrides");
        let code = self.rec_set_inner(set)?;
        Ok(if overrides {
            format!("apply_overrides({code})?")
        } else {
            code
        })
    }

    fn rec_set_inner(&mut self, mut set: SetNode) -> Res<String> {
        let dynamics = std::mem::take(&mut set.dynamics);
        let proxies = set.statics.contains_key("__overrides");
        let (decls, attrs) = self.group_with(set, proxies)?;
        let result = (|| -> Res<String> {
            let mut entries: Vec<(u32, u32, u64)> = attrs.iter().map(|(n, b, p)| (self.syms.sym(n), *b, *p)).collect();
            entries.sort_by_key(|e| e.0);
            let core = self.rec_entries(&entries)?;
            if dynamics.is_empty() {
                return Ok(format!("{{ {decls}{core} }}"));
            }
            let mut code = format!("{{ {decls}let mut a_ = (*force_attrs(&({core}))?).clone(); ");
            for (k, node, p) in &dynamics {
                let key = self.expr(k)?;
                let v = self.node_value(node, &[])?;
                let _ = write!(code, "insert_dynamic(&mut a_, &({key}), {v}, Pos({p}))?; ");
            }
            code.push_str("Value::attrs(a_) }");
            Ok(code)
        })();
        self.scopes.pop();
        result
    }
}

/// The amount of Nix source, in bytes, that one generated function takes
/// at most when a large set, list or group is split over several.
const CHUNK: usize = 16 * 1024;

/// The size of the source behind a node.
fn node_src_len(n: &Node) -> usize {
    match n {
        Node::Leaf(e, _) => u32::from(e.syntax().text_range().len()) as usize,
        Node::InheritOuter(..) | Node::InheritFrom(..) => 8,
        Node::Set(s) => {
            16 + s.statics.values().map(node_src_len).sum::<usize>()
                + s.dynamics.iter().map(|d| node_src_len(&d.1)).sum::<usize>()
        }
    }
}

/// Splits items into consecutive runs of about [`CHUNK`] bytes of source.
fn chunks_by<T>(items: &[T], len: impl Fn(&T) -> usize) -> Vec<&[T]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut acc = 0;
    for (i, x) in items.iter().enumerate() {
        let l = len(x);
        if acc > 0 && acc + l > CHUNK {
            out.push(&items[start..i]);
            start = i;
            acc = 0;
        }
        acc += l;
    }
    if start < items.len() {
        out.push(&items[start..]);
    }
    out
}

fn shift_temps(n: Node, offset: usize) -> Node {
    match n {
        Node::InheritFrom(t, name, p) => Node::InheritFrom(t + offset, name, p),
        other => other,
    }
}

/// The free variables of everything in a set node.
fn set_free_vars(s: &SetNode) -> HashSet<String> {
    let mut out = HashSet::new();
    for t in &s.temps {
        out.extend(free_vars(t));
    }
    for n in s.statics.values() {
        match n {
            Node::Leaf(e, _) => out.extend(free_vars(e)),
            Node::InheritOuter(name, _) => {
                out.insert(name.clone());
            }
            Node::InheritFrom(..) => {}
            Node::Set(s) => out.extend(set_free_vars(s)),
        }
    }
    for (k, n, _) in &s.dynamics {
        out.extend(free_vars(k));
        match n {
            Node::Leaf(e, _) => out.extend(free_vars(e)),
            Node::InheritOuter(name, _) => {
                out.insert(name.clone());
            }
            Node::InheritFrom(..) => {}
            Node::Set(s) => out.extend(set_free_vars(s)),
        }
    }
    out
}

/// The expression computing a dynamic attribute name.
fn dynamic_key(a: &ast::Attr) -> Res<ast::Expr> {
    match a {
        ast::Attr::Dynamic(d) => need(d.expr(), "attribute name"),
        ast::Attr::Str(s) => Ok(ast::Expr::Str(s.clone())),
        ast::Attr::Ident(_) => Err("internal error: static attribute treated as dynamic".to_string()),
    }
}

/// Whether any attribute path of a set has a dynamic component.
fn entries_dynamic(s: &ast::AttrSet) -> bool {
    s.attrpath_values()
        .any(|av| av.attrpath().is_some_and(|p| p.attrs().any(|a| static_attr_name(&a).is_none())))
}
