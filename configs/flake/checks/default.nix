# SPDX-License-Identifier: MIT
#
# The checks of nixpkgs-rust: a flake-parts configuration declared
# inside the flake-parts caisson top (configs/flake/nixpkgs-rust), whose
# dependencies are those of the caisson top with the pins of
# tests/dependencies beside them. The caisson top gathers the checks
# declared here into its `checks` output. A flake that lists
# nixpkgs-rust as an input reads nothing of this configuration, so it
# locks none of the pins of tests/dependencies.
{ ... }:
{ inputs, lib, ... }:
{
  imports = [ inputs.treefmt-nix.flakeModule ];

  perSystem =
    { pkgs, system, ... }:
    let
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
      # Duplicated in the formatter partition of
      # configs/flake/nixpkgs-rust: the partition and this configuration
      # are separate evaluations.
      treefmt.programs.nixfmt.enable = true;

      caisson.checks = {
        treefmt = {
          category = "format";
          description = "the Nix files are formatted with nixfmt";
        };

        # See the runtime's langtests.rs for what is compared.
        lang-tests = {
          derivation =
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
            pkgs.runCommand "nixpkgs-rust-lang-tests"
              {
                # Features compiled programs do not have; see
                # docs/development/product.md § Bounds.
                LANG_TESTS_KNOWN_FAILURES = lib.concatStringsSep " " [
                  "eval-okay-flake-ref-to-string"
                  "eval-okay-parse-flake-ref"
                  "eval-okay-import"
                ];
              }
              ''
                ${program}/bin/lang lang-tests | tee $out
              '';
          category = "build";
          description = "the language tests of the Nix in the package set, compiled and run, give the results Nix gives";
        };

        # A NixOS system takes sources through filters (lib.cleanSource
        # and the like), which packages such as hello do not.
        drv-parity = {
          derivation =
            let
              nixpkgsPath = scope.nixpkgs-rust.passthru.nixpkgsPath;
              cdModule = "${nixpkgsPath}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix";
              # The configuration is passed to both sides as the value of
              # `import <module>`: clone-config.nix records the modules a
              # system was built from, so passing a path to one side and
              # a value to the other would give different systems.
              nixos = import "${nixpkgsPath}/nixos" {
                inherit system;
                configuration = import cdModule;
              };
              compare = name: command: want: ''
                got=$(${scope.nixpkgs-rust}/bin/nixpkgs-rust ${command})
                want=${builtins.unsafeDiscardOutputDependency want}
                if [ "$got" != "$want" ]; then
                  echo "${name}: got $got, want $want" >&2
                  exit 1
                fi
                echo "${name}: $got" >> $out
              '';
            in
            pkgs.runCommand "nixpkgs-rust-drv-parity" { } (
              lib.concatMapStrings (
                a: compare a "instantiate -A ${a} --arg config '{}' --arg overlays '[]'" reference.${a}.drvPath
              ) parityAttrs
              +
                compare "nixos-installation-cd-minimal"
                  "instantiate nixos/default.nix -A system --arg configuration 'import ${cdModule}'"
                  nixos.system.drvPath
            );
          category = "build";
          description = "the derivations the compiled nixpkgs instantiates are the derivations Nix instantiates from the same nixpkgs";
        };
      };
    };
}
