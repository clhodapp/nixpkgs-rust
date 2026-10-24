# SPDX-License-Identifier: MIT
#
# The packages of this flake. `nixpkgs-rust` is the package set it is built
# from, compiled to Rust: a binary that evaluates, builds and installs that
# set's packages without the Nix evaluator.
{ callPackage, ... }:
let
  src = callPackage ./source.nix { };
  nixpkgs-rust-compiler = callPackage ./nixpkgs-rust-compiler { inherit src; };
  buildNixProgram = callPackage ./build-program.nix { inherit src nixpkgs-rust-compiler; };
in
{
  inherit nixpkgs-rust-compiler;
  nixpkgs-rust = callPackage ./nixpkgs-rust { inherit buildNixProgram; };
}
