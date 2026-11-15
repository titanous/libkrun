# The arm64 Nitro inputs of the random-copy test: random.c built static for aarch64 (musl) and a static
# aarch64 busybox, from the nixpkgs pinned by a holodeck tree.
# nix-build tests/cooperative/random-arm64.nix --arg holodeck /path/to/holodeck
{ holodeck }:
let
  lock = builtins.fromJSON (builtins.readFile (holodeck + "/flake.lock"));
  node = lock.nodes.nixpkgs.locked;
  source = builtins.fetchTree { type = "github"; inherit (node) owner repo rev narHash; };
  pkgs = import source { system = builtins.currentSystem; };
  cross = pkgs.pkgsCross.aarch64-multiplatform.pkgsStatic;
in cross.stdenv.mkDerivation {
  name = "random-copy-arm64";
  src = ./.;
  dontConfigure = true;
  buildPhase = "$CC -O2 -Wall -Wextra -Werror -static -pthread random.c -o random-arm64";
  installPhase = ''
    mkdir -p $out
    cp random-arm64 $out/
    cp ${cross.busybox}/bin/busybox $out/busybox-arm64
  '';
}
