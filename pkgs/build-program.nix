# SPDX-License-Identifier: MIT
#
# Compiles every Nix file under `root` to Rust and builds the result: a
# binary named `binName` whose `eval`, `instantiate`, `build`, `install`
# and `lang-tests` commands work on those files (see `cli.rs` in the
# runtime). The binary keeps `root` as the directory it reads files from.
#
# `<nix/fetchurl.nix>` is compiled from the source of the Nix in this
# package set, so that the derivations it produces are those Nix produces.
{
  lib,
  stdenv,
  cargo,
  rustc,
  rustPlatform,
  nix,
  src,
  nixpkgs-rust-compiler,
}:
{
  pname,
  root,
  binName ? pname,
  # File extensions compiled besides `nix`.
  alsoExt ? [ ],
  meta ? { },
  passthru ? { },
}:
stdenv.mkDerivation {
  inherit pname src meta passthru;
  version = "0.1.0";

  cargoDeps = rustPlatform.importCargoLock { lockFile = ../Cargo.lock; };
  nativeBuildInputs = [
    cargo
    rustc
    rustPlatform.cargoSetupHook
    nixpkgs-rust-compiler
  ];

  buildPhase = ''
    runHook preBuild
    nixpkgs-rust-compiler \
      --root ${root} \
      --out gen \
      --runtime "$PWD/crates/runtime" \
      --bin-name ${lib.escapeShellArg binName} \
      ${lib.concatMapStringsSep " " (e: "--also-ext ${lib.escapeShellArg e}") alsoExt} \
      --extra nix/fetchurl.nix=${nix.src}/src/libexpr/fetchurl.nix
    # The generated workspace needs the dependency versions of the runtime;
    # cargo adds the generated crates to the lock on its own.
    cp Cargo.lock gen/Cargo.lock
    (cd gen && cargo build --release --offline -j "$NIX_BUILD_CORES")
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -Dm755 gen/target/release/${lib.escapeShellArg binName} "$out/bin/${binName}"
    runHook postInstall
  '';
}
