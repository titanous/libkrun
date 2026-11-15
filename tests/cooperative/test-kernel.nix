# Test-variant guest kernel (cloneTests = true: USERFAULTFD + 0105) from a holodeck tree.
# nix-build tests/cooperative/test-kernel.nix --arg holodeck /path/to/holodeck
# --arg leakRandomReader true adds 0190-test-leak-random-reader.patch (beside this file): the kernel
# keeps 64 bytes per task that reads random bytes, the self-check that the memory-growth test of
# random-run.py --bomb can fail.
{ holodeck, arch ? "x86_64", leakRandomReader ? false }:
let
  lock = builtins.fromJSON (builtins.readFile (holodeck + "/flake.lock"));
  node = lock.nodes.nixpkgs.locked;
  nixpkgs = builtins.fetchTree { type = "github"; inherit (node) owner repo rev narHash; };
  pkgs = import nixpkgs { system = builtins.currentSystem; };
  holodeckPatches = holodeck + "/guest/kernel/patches";
  patchDirectory = if !leakRandomReader then holodeckPatches else
    pkgs.runCommand "guest-kernel-patches-leak-random-reader" { } ''
      mkdir $out
      cp ${holodeckPatches}/*.patch $out/
      cp ${./0190-test-leak-random-reader.patch} $out/0190-test-leak-random-reader.patch
    '';
in import (holodeck + "/guest/kernel") { inherit pkgs arch patchDirectory; cloneTests = true; }
