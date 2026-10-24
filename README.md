# nixpkgs-rust

nixpkgs compiled to Rust. A compiler turns every `.nix` file of nixpkgs into
Rust functions; the result builds into one binary, `nixpkgs-rust`, that
evaluates, builds and installs nixpkgs packages without the Nix evaluator.
It produces the same derivations Nix produces from the same nixpkgs, and
builds them through the Nix daemon.

```sh
nix run github:clhodapp/nixpkgs-rust -- build -A hello
nix run github:clhodapp/nixpkgs-rust -- install -A hello
```

| Command | Does |
|---|---|
| `instantiate -A ATTR` | prints the `.drv` path, as `nix-instantiate` does |
| `build -A ATTR` | builds through the daemon and links `./result`, as `nix-build` does |
| `install -A ATTR...` | adds packages to a profile (`$XDG_STATE_HOME/nixpkgs-rust/profile`), as `nix-env --install` does |
| `remove -A ATTR...`, `list` | removes from and lists the profile |
| `eval -A ATTR` | prints a value, as `nix-instantiate --eval --strict` does |

The binary is the nixpkgs of the package set it was built from (`pkgs.path`);
building this flake with another nixpkgs compiles that one.

What it does not do: parse Nix at run time (so `--arg` takes literals and
`import PATH` only), `builtins.scopedImport`, network fetchers during
evaluation, flakes, and experimental Nix features. See
[`docs/development/product.md`](docs/development/product.md).

The idea is from jinx, which compiles nixpkgs to TypeScript
([nixpkgs for the frontend impaired](https://fzakaria.com/2026/10/12/nixpkgs-for-the-frontend-impaired)).
