# SPDX-License-Identifier: MIT
#
# The Cargo workspace: the compiler and the runtime its output links against.
{ lib }:
lib.fileset.toSource {
  root = ../.;
  fileset = lib.fileset.unions [
    ../Cargo.toml
    ../Cargo.lock
    ../crates
  ];
}
