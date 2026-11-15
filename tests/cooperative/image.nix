# nix build --impure --file tests/cooperative/image.nix --arg holodeck /path/to/holodeck
{ holodeck, workload ? "devices" }:
let
  lock = builtins.fromJSON (builtins.readFile (holodeck + /flake.lock));
  node = lock.nodes.nixpkgs.locked;
  source = builtins.fetchTree {
    type = "github";
    inherit (node) owner repo rev narHash;
  };
  pkgs = import source { system = builtins.currentSystem; };
  init = pkgs.pkgsStatic.stdenv.mkDerivation {
    name = "cooperative-devices-init";
    src = ./.;
    dontConfigure = true;
    buildPhase = "$CC -O2 -Wall -Wextra -Werror -static -pthread ${workload}.c -o init";
    installPhase = "mkdir -p $out; cp init $out/init";
  };
in import (holodeck + /guest/image) {
  inherit pkgs;
  initBinary = "${init}/init";
  # The random workload runs an ordinary shell loop of short processes before each freeze.
  extraFiles = pkgs.lib.optional (workload == "random")
    { destination = "bin/busybox"; source = "${pkgs.pkgsStatic.busybox}/bin/busybox"; };
}
