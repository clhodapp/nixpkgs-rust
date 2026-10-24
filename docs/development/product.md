# nixpkgs-rust: product

## What it is

nixpkgs-rust compiles nixpkgs to Rust. The compiler turns every `.nix` file
of a nixpkgs tree into Rust functions in a generated Cargo workspace; that
workspace builds into one binary, `nixpkgs-rust`, which evaluates, builds
and installs nixpkgs packages without the Nix evaluator:

```text
nixpkgs-rust instantiate -A hello      # the .drv path, as nix-instantiate prints it
nixpkgs-rust build -A hello            # build through the Nix daemon, ./result link
nixpkgs-rust install -A hello -A jq    # add to a profile, as nix-env --install
nixpkgs-rust remove -A jq
nixpkgs-rust list
nixpkgs-rust eval -A lib.version       # print a value, as nix-instantiate --eval --strict
```

The idea comes from jinx, which compiles nixpkgs to TypeScript
(<https://fzakaria.com/2026/10/12/nixpkgs-for-the-frontend-impaired>).
nixpkgs-rust targets Rust instead and adds building and installing.

## Goals

- **The same derivations as Nix.** For the nixpkgs it was compiled from,
  `instantiate` prints the derivation path `nix-instantiate` prints. The
  `drv-parity` check holds this for a set of packages.
- **Nix language semantics**, measured by the language tests of Nix itself
  (`tests/functional/lang`), which the `lang-tests` check runs.
- **Building and installing** through the Nix daemon: the binary adds the
  derivations and sources it computed to the store and asks the daemon to
  build; the daemon builds or substitutes the dependencies.

## Bounds

- A compiled program evaluates only the files it was compiled from. It
  cannot parse Nix at run time, so `--arg` takes only literals and
  `import PATH`, and `builtins.scopedImport` is not supported.
- No fetchers that evaluate with network access (`builtins.fetchurl`,
  `fetchTarball`, `fetchGit`, `fetchTree`) and no flakes (`getFlake`,
  `parseFlakeRef`, `flakeRefToString`). nixpkgs does not use these to
  evaluate packages.
- Experimental Nix features (content-addressed and impure derivations,
  dynamic derivations, Git hashing, TOML timestamps) are reported as
  disabled, as a default Nix reports them.
- Lints (`--lint-*`) are not implemented.

## Where it fits

The workspace vision asks for eval-time profiling of the composition layer
(`docs/development/vision.md` § Lower). Compiled Nix runs as native code, so
the ordinary Rust profiling tools (`perf`, flame graphs) apply to it; each
generated function corresponds to one lazy expression of the source.
