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
let
  # Interpolating `path` would copy the tree to a new store path named after
  # the old one, and the binary would then not recognise imports of the
  # original tree. Copying it under its own name yields the same store path.
  base = baseNameOf (toString path);
  storeName = if builtins.match "[0-9a-z]{32}-.*" base != null then builtins.substring 33 (-1) base else base;
  root = builtins.path {
    inherit path;
    name = storeName;
  };
in
buildNixProgram {
  pname = "nixpkgs-rust";
  inherit root;
  # The nixpkgs tree that was compiled, as a store path string, for
  # comparisons with Nix. Interpolating it does not copy the tree again.
  passthru.nixpkgsPath = root;
  meta = {
    description = "nixpkgs compiled to Rust: evaluates, builds and installs its packages";
    mainProgram = "nixpkgs-rust";
    platforms = lib.platforms.linux;
  };
}
