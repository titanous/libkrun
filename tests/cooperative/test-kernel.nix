# Test-variant guest kernel (cloneTests = true: USERFAULTFD + 0105) from a holodeck tree.
# nix-build tests/cooperative/test-kernel.nix --arg holodeck /path/to/holodeck
{ holodeck, arch ? "x86_64" }:
let
  lock = builtins.fromJSON (builtins.readFile (holodeck + "/flake.lock"));
  node = lock.nodes.nixpkgs.locked;
  nixpkgs = builtins.fetchTree { type = "github"; inherit (node) owner repo rev narHash; };
  pkgs = import nixpkgs { system = builtins.currentSystem; };
in import (holodeck + "/guest/kernel") { inherit pkgs arch; cloneTests = true; }
