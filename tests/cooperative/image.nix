# nix build --impure --file tests/cooperative/image.nix --arg holodeck /path/to/holodeck
{ holodeck }:
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
    buildPhase = "$CC -O2 -Wall -Wextra -Werror -static -pthread devices.c -o init";
    installPhase = "mkdir -p $out; cp init $out/init";
  };
in import (holodeck + /guest/image) {
  inherit pkgs;
  initBinary = "${init}/init";
}
