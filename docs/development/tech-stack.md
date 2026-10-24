# nixpkgs-rust: tech stack

Extends the workspace `docs/development/tech-defaults.md`. Unlike most
workspace projects, the product is Rust; Nix packages it.

## Crates

- `crates/compiler` (`nixpkgs-rust-compiler`): parses with `rnix` and
  writes the generated workspace.
- `crates/runtime` (`nixpkgs-rust-runtime`): what generated code links
  against: values, thunks, operators, the built-in functions, store path
  computation, derivations, the Nix daemon client, the command line.

## How Nix becomes Rust

- Every expression that must stay lazy becomes a *unit*: a Rust function
  `fn(e: &[Value], a: Value) -> R<Value>`, where `e` holds the values the
  unit captured and `a` is the argument of a lambda body. Cheap lazy forms
  (literals, variables, lambdas, lists, static sets, selections and
  applications of those) are built from runtime thunk kinds instead.
- Variables resolve lexically, then to the global builtins, then through
  enclosing `with` scopes at run time, which is the order Nix binds them in.
- `let` and recursive sets build bindings in dependency order and allocate
  the rest as blank thunks filled in afterwards. Large sets, lists and
  groups are built in chunks of about 16 KB of Nix source per function,
  because rustc time grows with function size.
- Units are placed into generated crates of about 2 MB each. Units refer to
  each other through `CodeId` (crate and index into the crate's table of
  functions), so the generated crates do not depend on each other and
  compile in parallel. nixpkgs becomes about 214 crates, 428 MB of Rust.
- Positions are a file id and a byte offset; lines and columns are computed
  from the source file when needed.

## Semantics references

Behaviour follows Nix 2.34: `libexpr` for evaluation and the builtins,
`libstore/derivations.cc` for the derivation format and modulo hashing,
the worker protocol (version 1.37) for the daemon. Details that are easy to
get wrong are pinned by unit tests against values observed from Nix:
string escaping and indentation, float printing (`%g`, nlohmann JSON),
regular expressions (POSIX leftmost-longest with bracket rules), store paths.

## Build profile

Generated crates compile without optimisation (`opt-level = 0`); the
runtime compiles with `opt-level = 3`. Optimising 428 MB of generated code
would multiply the build time for little gain, as evaluation time is mostly
spent in the runtime.
