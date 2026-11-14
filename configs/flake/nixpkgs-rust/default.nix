# SPDX-License-Identifier: MIT
{ ... }:
{
  config,
  inputs,
  lib,
  ...
}:
{

  imports = [ inputs.flake-parts.flakeModules.partitions ];

  debug = false;
  caisson = {
    libOverlays.exported = libOverlays: {
      inherit (libOverlays) default;
    };

    # The packages are the `default` entry of this flake's package overlay
    # registry (pkg-overlays/): the package set applies it by default, and
    # the flake exports it as `pkgOverlays` and as `overlays.default`.
    nixpkgs = {
      packages.export.enabled = true;
    };
  };

  # `nix run github:clhodapp/nixpkgs-rust -- build -A hello` reaches the
  # compiled nixpkgs, which is what anyone arrives here for.
  perSystem =
    { config, ... }:
    {
      packages.default = config.packages.nixpkgs-rust;
    };

  # The checks of nixpkgs-rust, with the pins of tests/dependencies that
  # only the checks need (configs/flake/checks). The caisson top gathers
  # the checks of every configuration declared inside it into its
  # `checks` output, the build of each package among them. This selector
  # passes the configuration up to nothing, so a flake that reads the
  # exports of nixpkgs-rust evaluates neither the configuration nor its
  # pins.
  caisson.flake-parts.configurations.checks = lib.caisson.flake-parts.mkConfiguration {
    inherit (lib.caisson.pins.flake-compat ../../../tests/dependencies) dependencies;
  };
  caisson.flake-parts.exported = _: { };

  partitionedAttrs.formatter = "formatter";

  partitions.formatter = {
    extraInputs = (lib.caisson.pins.flake-compat ../../../tests/dependencies).dependencies;
    module =
      { inputs, ... }:
      {
        imports = [ inputs.treefmt-nix.flakeModule ];
        perSystem.treefmt.programs.nixfmt.enable = true;
      };
  };

}
