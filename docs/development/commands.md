# nixpkgs-rust: commands

Run from the project directory.

```zsh
nix build                                   # the compiled nixpkgs, ./result/bin/nixpkgs-rust
nix build '.#nixpkgs-rust-compiler'         # the compiler alone
nix flake check                             # every check, see testing.md
nix build '.#checks.x86_64-linux.drv-parity' -L
nix fmt
```

Using the binary (any directory):

```zsh
nixpkgs-rust build -A hello                 # builds through the Nix daemon, links ./result
nixpkgs-rust install -A hello               # profile: $XDG_STATE_HOME/nixpkgs-rust/profile
nixpkgs-rust list
nixpkgs-rust remove -A hello
```

`--profile PATH` picks another profile; `NIXPKGS_RUST_PROFILE` sets the
default. `NIXPKGS_RUST_ROOT` points the binary at another copy of the
nixpkgs tree it was compiled from, and `NIXPKGS_RUST_SYSTEM` overrides
`builtins.currentSystem`. `-v` shows the daemon's build log.
