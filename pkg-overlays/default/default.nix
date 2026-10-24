# SPDX-License-Identifier: MIT
#
# The default package overlay: the packages of pkgs/ under
# `pkgs.nixpkgs-rust` (`.nixpkgs-rust`, `.nixpkgs-rust-compiler`). A
# consumer that lists this flake in `projects` holds it as
# `nixpkgs-rust/default`, which its package sets apply by default.
{ closure-lib, ... }:
{
  overlay = closure-lib.caisson.nixpkgs.mkPackagesOverlay ../../pkgs "nixpkgs-rust";
}
