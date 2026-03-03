{
  description = "libkrun development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # The NixOS pkg-config wrapper sets NIX_PKG_CONFIG_WRAPPER_TARGET_TARGET_*
        # (triggered by glibc.dev in buildInputs) which causes it to replace
        # PKG_CONFIG_PATH with PKG_CONFIG_PATH_x86_64_unknown_linux_gnu (empty).
        # This shim syncs PKG_CONFIG_PATH into the target-specific var first,
        # so the wrapper finds whatever path cargo or run.sh sets at runtime.
        pkgConfigShim = pkgs.writeShellScript "pkg-config-shim" ''
          export PKG_CONFIG_PATH_x86_64_unknown_linux_gnu="''${PKG_CONFIG_PATH}''${PKG_CONFIG_PATH:+:}''${PKG_CONFIG_PATH_x86_64_unknown_linux_gnu:-}"
          exec ${pkgs.pkg-config}/bin/pkg-config "$@"
        '';

        # Rebuild libkrunfw 5.2.1 with ACPI + VMGENID enabled in the guest kernel.
        # Upstream config has `# CONFIG_ACPI is not set`; we append the needed
        # options and let `make olddefconfig` resolve dependencies.
        libkrunfw-acpi = pkgs.libkrunfw.overrideAttrs (old: {
          version = "5.2.1";
          src = pkgs.fetchFromGitHub {
            owner = "containers";
            repo = "libkrunfw";
            tag = "v5.2.1";
            hash = "sha256-hRu9HEWTyToqntDkqBIvWEn+kAidQdspyWc6Le587qw=";
          };
          kernelSrc = pkgs.fetchurl {
            url = "mirror://kernel/linux/kernel/v6.x/linux-6.12.68.tar.xz";
            hash = "sha256-02fHUEvU2lIN0B6wgSXS0KwIi8ivTNVtI28gdN1CJbc=";
          };
          postPatch = (old.postPatch or "") + ''
            cat >> config-libkrunfw_x86_64 <<'ACPI_EOF'
CONFIG_ACPI=y
CONFIG_PCI=y
CONFIG_VMGENID=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_SERIAL_EARLYCON=y
ACPI_EOF
          '';
        });
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            # Rust toolchain (resolved from rust-toolchain.toml via rust-overlay)
            toolchain

            # C compiler and linker (required for the cdylib output)
            gcc
            gnumake

            # pkg-config (used by rutabaga_gfx/build.rs and krun-sys/build.rs)
            pkg-config

            # libclang: loaded at runtime by bindgen (bindgen_clang_runtime feature)
            # krun_input and krun_display build.rs use this
            llvmPackages.libclang

            # glibc headers for bindgen: NixOS has no /usr/include,
            # so clang needs an explicit path via BINDGEN_EXTRA_CLANG_ARGS
            glibc.dev

            # libcap-ng: linked by the capng Rust crate (upstream dependency)
            libcap_ng

            # ifconfig: used by tests/run.sh to configure loopback in network namespace
            nettools

            # VM firmware with ACPI+VMGENID kernel support; loaded at runtime by libkrun
            libkrunfw-acpi

            # for --features snd (virtio-snd pipewire backend)
            pipewire.dev

            # for --features gpu (virtio-gpu with virglrenderer)
            virglrenderer
            libepoxy.dev
            libdrm.dev

            # stress testing for flakiness investigation
            stress-ng
          ];

          # Point Rust's pkg_config crate at the shim so PKG_CONFIG_PATH set by
          # cargo or run.sh is forwarded to the NixOS wrapper's target-specific var.
          PKG_CONFIG = pkgConfigShim;

          # NixOS has no /usr/include; tell clang where glibc headers live.
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include -isystem ${pkgs.pipewire.dev}/include";

          # Force gcc as the host linker. The rust-overlay toolchain defaults to
          # its bundled LLD, which cannot resolve glibc's open64/stat64 compat
          # aliases on glibc 2.34+ (NixOS). gcc delegates to GNU ld which handles
          # these correctly.
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "${pkgs.gcc}/bin/gcc";

          # Linker for the x86_64-unknown-linux-musl target (guest-agent)
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER =
            "${pkgs.pkgsMusl.stdenv.cc}/bin/cc";

          shellHook = ''
            # `make test` hardcodes LD_LIBRARY_PATH to test-prefix/lib64 only.
            # Symlink libkrunfw there so the test runner can find it alongside libkrun.
            mkdir -p test-prefix/lib64
            for lib in ${libkrunfw-acpi}/lib64/libkrunfw*; do
              ln -sf "$lib" "$(pwd)/test-prefix/lib64/$(basename "$lib")"
            done

            # init/init.c must be statically linked; NixOS has no static glibc.
            # Use the musl toolchain instead. The Makefile uses CC_LINUX=$(CC) on Linux.
            # Set here (in shellHook, after setup hooks run) to override buildInputs CC.
            export CC="${pkgs.pkgsMusl.stdenv.cc}/bin/cc"
          '';
        };
      }
    );
}
