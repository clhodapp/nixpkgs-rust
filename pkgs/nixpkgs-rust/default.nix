# SPDX-License-Identifier: MIT
#
# `nixpkgs-rust`: the nixpkgs this package set comes from (`pkgs.path`),
# compiled to Rust. The binary evaluates, builds and installs packages of
# that nixpkgs without the Nix evaluator; it builds through the Nix daemon,
# and reads the nixpkgs tree from the store for everything other than
# evaluating its Nix files.
{
  lib,
  path,
  buildNixProgram,
}:
buildNixProgram {
  pname = "nixpkgs-rust";
  root = path;
  # The nixpkgs tree that was compiled, for comparisons with Nix.
  passthru.nixpkgsPath = path;
  meta = {
    description = "nixpkgs compiled to Rust: evaluates, builds and installs its packages";
    mainProgram = "nixpkgs-rust";
    platforms = lib.platforms.linux;
  };
}
