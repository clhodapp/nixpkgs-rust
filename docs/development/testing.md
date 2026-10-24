# nixpkgs-rust: testing

All checks run with `nix flake check` from the project directory.

| Check | What it shows |
|---|---|
| `nixpkgs-rust-compiler` | The Rust unit tests of both crates (its check phase). |
| `lang-tests` | The Nix language tests (`tests/functional/lang` of the Nix in the package set), compiled and run: each `eval-okay` test prints its `.exp`, each `eval-fail` test fails. Tests that need lints or experimental features are skipped. |
| `drv-parity` | For a set of packages, `nixpkgs-rust instantiate` prints the derivation path Nix computes for the same nixpkgs. |
| `treefmt` | Nix formatting. |

Known failures of `lang-tests`: `eval-okay-flake-ref-to-string`,
`eval-okay-parse-flake-ref` and `eval-okay-import` (which uses
`builtins.scopedImport`); see `product.md` § Bounds.

## Locally, without Nix builds

The generated nixpkgs program takes about 6.5 minutes to build with 16
jobs and about 1 GB of memory per job. To iterate on the runtime, compile a
small tree instead, such as nixpkgs' `lib` directory, whose own tests
(`lib/tests/misc.nix`) print `[ ]` when they pass:

```zsh
cargo run --release -p nixpkgs-rust-compiler -- --root <nixpkgs>/lib --out gen-lib --runtime crates/runtime
cd gen-lib && cargo build --offline && target/debug/nixpkgs-rust eval tests/misc.nix
```
