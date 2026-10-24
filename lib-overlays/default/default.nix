# SPDX-License-Identifier: MIT
{ ... }:
{

  overlay = final: prev: {
    nixpkgs-rust = prev.nixpkgs-rust or { };
  };

}
