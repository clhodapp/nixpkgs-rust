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

  partitionedAttrs.checks = "checks";
  partitionedAttrs.formatter = "formatter";

  partitions.formatter = {
    extraInputs = (lib.caisson.pins.flake-compat ../../../tests/dependencies).sources;
    module =
      { inputs, ... }:
      {
        imports = [ inputs.treefmt-nix.flakeModule ];
        perSystem.treefmt.programs.nixfmt.enable = true;
      };
  };

  partitions.checks = {
    extraInputs = (lib.caisson.pins.flake-compat ../../../tests/dependencies).sources;
    module =
      { inputs, self, ... }:
      {
        imports = [ inputs.treefmt-nix.flakeModule ];
        perSystem =
          { pkgs, system, ... }:
          let
            packages = self.packages.${system};
            scope = pkgs.nixpkgs-rust;
            # The nixpkgs `nixpkgs-rust` compiled, as Nix itself evaluates it.
            reference = import scope.nixpkgs-rust.passthru.nixpkgsPath {
              inherit system;
              config = { };
              overlays = [ ];
            };
            parityAttrs = [
              "hello"
              "jq"
              "python3"
              "git"
            ];
          in
          {
            checks = {
              # The compiler and runtime unit tests run in its check phase.
              inherit (packages) nixpkgs-rust-compiler;

              # The language tests of the Nix in this package set, compiled
              # and run; see the runtime's langtests.rs for what is compared.
              lang-tests =
                let
                  buildNixProgram = pkgs.callPackage ../../../pkgs/build-program.nix {
                    src = pkgs.callPackage ../../../pkgs/source.nix { };
                    inherit (scope) nixpkgs-rust-compiler;
                  };
                  program = buildNixProgram {
                    pname = "nix-lang-tests";
                    binName = "lang";
                    root = "${pkgs.nix.src}/tests/functional/lang";
                    alsoExt = [ "exp" ];
                  };
                in
                pkgs.runCommand "nixpkgs-rust-lang-tests" { } ''
                  ${program}/bin/lang lang-tests | tee $out
                '';

              # The derivations the compiled nixpkgs instantiates are the
              # derivations Nix instantiates from the same nixpkgs.
              drv-parity = pkgs.runCommand "nixpkgs-rust-drv-parity" { } (
                lib.concatMapStrings (a: ''
                  got=$(${packages.nixpkgs-rust}/bin/nixpkgs-rust instantiate -A ${a} \
                    --arg config '{}' --arg overlays '[]')
                  want=${builtins.unsafeDiscardOutputDependency reference.${a}.drvPath}
                  if [ "$got" != "$want" ]; then
                    echo "${a}: got $got, want $want" >&2
                    exit 1
                  fi
                  echo "${a}: $got" >> $out
                '') parityAttrs
              );
            };
            treefmt.programs.nixfmt.enable = true;
          };
      };
  };

}
