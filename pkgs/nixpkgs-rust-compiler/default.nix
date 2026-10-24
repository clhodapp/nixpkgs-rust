# SPDX-License-Identifier: MIT
#
# `nixpkgs-rust-compiler`: turns a tree of Nix files into a Cargo workspace.
# Its unit tests (string handling, and the runtime's hashing, store paths,
# regular expressions and printing) run in the check phase.
{ rustPlatform, src }:
rustPlatform.buildRustPackage {
  pname = "nixpkgs-rust-compiler";
  version = "0.1.0";
  inherit src;
  cargoLock.lockFile = ../../Cargo.lock;
  cargoBuildFlags = [
    "--package"
    "nixpkgs-rust-compiler"
  ];
  cargoTestFlags = [ "--workspace" ];
  meta = {
    description = "Compiler from Nix expressions to Rust";
    mainProgram = "nixpkgs-rust-compiler";
  };
}
