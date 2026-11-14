{ holodeck, fsHolodeck }:
let
  lock = builtins.fromJSON (builtins.readFile (holodeck + /flake.lock));
  node = lock.nodes.nixpkgs.locked;
  source = builtins.fetchTree { type = "github"; inherit (node) owner repo rev narHash; };
  pkgs = import source { system = builtins.currentSystem; };
  patches = pkgs.runCommand "cooperative-with-fs-patches" {} ''
    mkdir -p $out
    cp ${holodeck}/guest/kernel/patches/*.patch $out/
    cp ${fsHolodeck}/guest/kernel/patches/0200-*.patch $out/
    cp ${fsHolodeck}/guest/kernel/patches/0201-*.patch $out/
  '';
in import (holodeck + /guest/kernel) { inherit pkgs; patchDirectory = patches; }
