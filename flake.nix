# SPDX-License-Identifier: MIT
{

  description = "nixpkgs compiled to Rust: a binary that evaluates, builds and installs nixpkgs packages";

  # Honored only when this flake is evaluated directly and the settings are
  # accepted; a consumer that takes this flake as an input must declare the
  # caches itself.
  nixConfig = {
    extra-substituters = [
      "https://clhodapp.cachix.org"
      "https://nix-community.cachix.org"
      "https://numtide.cachix.org"
    ];
    extra-trusted-public-keys = [
      "clhodapp.cachix.org-1:EW/0conxH0OQyo0o4ub/grdkFspholmQMSnQyj0vrZI="
      "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs="
      "numtide.cachix.org-1:2ps1kLBUWjxIneOy1Ik6cQjb41X0iXVXeHigGmycPPE="
    ];
  };

  inputs = {
    caisson.url = "github:nix-caisson/caisson";
    # The package set the tools are built from, and the nixpkgs that
    # `nixpkgs-rust` compiles.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # The library comes from the nixpkgs.lib mirror.
    nixpkgs-lib.url = "github:nix-community/nixpkgs.lib";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs-lib";
  };

  outputs =
    inputs@{ caisson, ... }:
    let
      lib = caisson.lib.caisson.mkLib {
        inherit (caisson.lib.caisson.pins.flake inputs) sources root;
        name = "nixpkgs-rust";
        systems = [
          "x86_64-linux"
          "aarch64-linux"
        ];
        projects = {
          inherit caisson;
        };

        configs = lib: lib.caisson.mkModules ./configs;

        pkgSets = lib: lib.caisson.nixpkgs.mkConfigurations { };

        libOverlays = lib: lib.caisson.mkLibOverlays ./lib-overlays;
        pkgOverlays = lib: lib.caisson.mkPkgOverlays ./pkg-overlays;
      };
    in
    lib.caisson.flake-parts.mkTopConfiguration { };

}
