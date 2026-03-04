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

        # Nightly toolchain for miri, asan, and cargo-fuzz.
        # Extensions: miri (interpreter), rust-src (needed by miri/asan), llvm-tools-preview (sanitizer runtime).
        nightlyToolchain = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [ "miri" "rust-src" "llvm-tools-preview" ];
        };

        # Kani requires an exact nightly to match the kani-compiler binary in its release bundle.
        # kani 0.67.0 was built against nightly-2025-11-21 (rustc 1.93.0-nightly 53732d5e0).
        # This toolchain provides librustc_driver-b64ee523d8950218.so for kani-compiler's RUNPATH.
        kaniNightlyToolchain = pkgs.rust-bin.nightly."2025-11-21".default.override {
          extensions = [ "rust-src" ];
        };

        # Kani release bundle: pre-built kani-driver, kani-compiler, cbmc, goto-cc, kissat.
        # autoPatchelfHook rewrites the ELF interpreter and rpath for the Nix glibc.
        # kaniNightlyToolchain in buildInputs provides librustc_driver for kani-compiler
        # (RUNPATH originally points to $ORIGIN/../toolchain/lib and /home/runner/.rustup/...).
        # stdenv.cc.cc.lib provides libstdc++.so.6 required by cbmc and goto-* (C++ binaries).
        kaniBundle = pkgs.stdenv.mkDerivation {
          name = "kani-0.67.0";
          src = pkgs.fetchurl {
            url = "https://github.com/model-checking/kani/releases/download/kani-0.67.0/kani-0.67.0-x86_64-unknown-linux-gnu.tar.gz";
            hash = "sha256-O196/TtRYD7nINt7wbxP5GtaT1022q2ZOcS0xli1GsA=";
          };
          nativeBuildInputs = [ pkgs.autoPatchelfHook ];
          buildInputs = [
            pkgs.stdenv.cc.cc.lib  # libstdc++.so.6 for cbmc / goto-* (C++)
            kaniNightlyToolchain   # librustc_driver-b64ee523d8950218.so for kani-compiler
          ];
          dontBuild = true;
          # unpackPhase leaves us inside kani-0.67.0/ (the tarball's top-level dir).
          # Copy to $out/kani-0.67.0/ so KANI_HOME=$out satisfies kani_dir() = $KANI_HOME/kani-0.67.0.
          # kani-driver finds cargo at toolchain/bin/cargo; symlink kaniNightlyToolchain there.
          installPhase = ''
            mkdir -p $out/kani-0.67.0
            cp -r . $out/kani-0.67.0/
            ln -s ${kaniNightlyToolchain} $out/kani-0.67.0/toolchain
          '';
        };

        # cargo-kani proxy: bridges `cargo kani` to kani-driver in kaniBundle.
        # Replaces `cargo install --locked kani-verifier` + `cargo kani setup` without internet.
        # Cargo invokes as: cargo-kani kani [proof-args...]; "kani" is kani-driver's <INPUT>
        # subcommand and must NOT be stripped — pass all args through verbatim.
        kaniWrapper = pkgs.writeShellScriptBin "cargo-kani" ''
          KANI_DIR="${kaniBundle}/kani-0.67.0"

          # kani-compiler must match the nightly it was built against.
          # cargoWrapper intercepts RUSTUP_TOOLCHAIN=nightly-* → kaniNightlyToolchain.
          export RUSTUP_TOOLCHAIN="nightly-2025-11-21-x86_64-unknown-linux-gnu"

          # cbmc, goto-cc, kani-compiler are co-located with kani-driver.
          export PATH="$KANI_DIR/bin:$PATH"

          # KANI_HOME lets kani-driver locate its siblings and lets any re-entrant
          # cargo-kani invocations skip the setup check (appears_setup → dir exists).
          export KANI_HOME="${kaniBundle}"

          # kani-driver uses cargo_metadata which finds cargo via $CARGO, not PATH.
          # Point it at our cargoWrapper so RUSTUP_TOOLCHAIN dispatch works.
          export CARGO="${cargoWrapper}/bin/cargo"

          exec "$KANI_DIR/bin/kani-driver" "$@"
        '';

        # clang++ wrapper that fixes the glibc #include_next issue for C++ builds.
        #
        # Problem: glibc.dev setup hook adds glibc as `-isystem` before gcc's C++
        # headers in both NIX_CFLAGS_COMPILE and NIX_CFLAGS_COMPILE_FOR_TARGET.
        # The NixOS clang wrapper's libc-cflags correctly adds glibc as `-idirafter`
        # (which would appear after gcc C++ headers and make `#include_next <stdlib.h>`
        # work), but the gcc deduplication logic removes the -idirafter entry when the
        # same path is already listed as -isystem — leaving glibc before gcc C++ headers,
        # where #include_next from <cstdlib> cannot reach it.
        # When --target=x86_64-unknown-linux-gnu is set (e.g. fuzz/cc crate), the clang
        # wrapper reads NIX_CFLAGS_COMPILE_FOR_TARGET in addition to NIX_CFLAGS_COMPILE.
        #
        # Fix: strip glibc -isystem entries from both vars before calling the real
        # clang wrapper, so its -idirafter is not deduplicated away.
        cxxWrapper = pkgs.writeShellScriptBin "clang++" ''
          NIX_CFLAGS_COMPILE=$(
            echo "''${NIX_CFLAGS_COMPILE:-}" \
              | sed 's/ -isystem [^ ]*glibc[^ ]*-dev\/include//g'
          )
          export NIX_CFLAGS_COMPILE
          NIX_CFLAGS_COMPILE_FOR_TARGET=$(
            echo "''${NIX_CFLAGS_COMPILE_FOR_TARGET:-}" \
              | sed 's/ -isystem [^ ]*glibc[^ ]*-dev\/include//g'
          )
          export NIX_CFLAGS_COMPILE_FOR_TARGET
          exec "${pkgs.llvmPackages.clang}/bin/clang++" "$@"
        '';

        # Cargo wrapper that dispatches `cargo +nightly` / `cargo +stable` to the
        # corresponding Nix-provided toolchain binary without needing rustup.
        # Cargo finds its sibling rustc via the executable's own directory, so no
        # RUSTC override is needed.
        cargoWrapper = pkgs.writeShellScriptBin "cargo" ''
          case "$1" in
            +nightly)
              shift
              # Prepend nightly bin to PATH so cargo can find cargo-miri, cargo-fuzz, etc.
              exec env "PATH=${nightlyToolchain}/bin:$PATH" "${nightlyToolchain}/bin/cargo" "$@"
              ;;
            +stable)
              shift
              exec "${toolchain}/bin/cargo" "$@"
              ;;
            *)
              # Also honour RUSTUP_TOOLCHAIN so that env-var based dispatch (e.g.
              # integration-asan's run.sh) works without rustup installed.
              case "''${RUSTUP_TOOLCHAIN:-}" in
                nightly)
                  exec env "PATH=${nightlyToolchain}/bin:$PATH" "${nightlyToolchain}/bin/cargo" "$@"
                  ;;
                nightly-*)
                  # kani sets RUSTUP_TOOLCHAIN=nightly-2025-11-21-x86_64-unknown-linux-gnu;
                  # dispatch to the pinned kani nightly toolchain.
                  exec env "PATH=${kaniNightlyToolchain}/bin:$PATH" "${kaniNightlyToolchain}/bin/cargo" "$@"
                  ;;
                *)
                  exec "${toolchain}/bin/cargo" "$@"
                  ;;
              esac
              ;;
          esac
        '';

        # The NixOS pkg-config wrapper sets NIX_PKG_CONFIG_WRAPPER_TARGET_TARGET_*
        # (triggered by glibc.dev in buildInputs) which causes it to replace
        # PKG_CONFIG_PATH with PKG_CONFIG_PATH_x86_64_unknown_linux_gnu (empty).
        # This shim syncs PKG_CONFIG_PATH into the target-specific var first,
        # so the wrapper finds whatever path cargo or run.sh sets at runtime.
        pkgConfigShim = pkgs.writeShellScript "pkg-config-shim" ''
          export PKG_CONFIG_PATH_x86_64_unknown_linux_gnu="''${PKG_CONFIG_PATH}''${PKG_CONFIG_PATH:+:}''${PKG_CONFIG_PATH_x86_64_unknown_linux_gnu:-}"
          exec ${pkgs.pkg-config}/bin/pkg-config "$@"
        '';

        # Rebuild libkrunfw 5.2.1 / Linux 6.12.74 with VMGENID support via the
        # SETUP_VMGENID setup_data boot protocol (no ACPI required).
        # The kernel patch adds SETUP_VMGENID type 10 and a platform device
        # initcall; the vmgenid driver probes the platform device directly.
        libkrunfw-vmgenid = pkgs.libkrunfw.overrideAttrs (old: {
          version = "5.2.1";
          src = pkgs.fetchFromGitHub {
            owner = "containers";
            repo = "libkrunfw";
            tag = "v5.2.1";
            hash = "sha256-hRu9HEWTyToqntDkqBIvWEn+kAidQdspyWc6Le587qw=";
          };
          kernelSrc = pkgs.fetchurl {
            url = "mirror://kernel/linux/kernel/v6.x/linux-6.12.74.tar.xz";
            hash = "sha256-O1busdyaQ38YnKVrgjvjdpmU9ZpOoIlbCOwNIKysoT4=";
          };
          postPatch = (old.postPatch or "") + ''
            substituteInPlace Makefile \
              --replace 'KERNEL_VERSION = linux-6.12.68' 'KERNEL_VERSION = linux-6.12.74'

            cp ${./libkrunfw-patches/0022-vmgenid-setup-data.patch} patches/0022-vmgenid-setup-data.patch
            cp ${./libkrunfw-patches/0023-no-jitterentropy.patch} patches/0023-no-jitterentropy.patch
            cp ${./libkrunfw-patches/0024-virtio-mmio-async-probe.patch} patches/0024-virtio-mmio-async-probe.patch
            cat >> config-libkrunfw_x86_64 <<'KCONFIG_EOF'
CONFIG_VMGENID=y
# Remove jitterentropy (~14ms savings): 0023-no-jitterentropy.patch removes the unconditional
# `select CRYPTO_JITTERENTROPY` from CRYPTO_DRBG in crypto/Kconfig; without the select,
# the base config's explicit CONFIG_CRYPTO_JITTERENTROPY=y can be overridden here.
# CONFIG_RANDOM_TRUST_CPU=y ensures DRBG has CPU entropy (RDRAND) without needing jent.
CONFIG_CRYPTO_JITTERENTROPY=n
# Serial 8250 disabled in production (~12ms savings for serial8250_init).
# For earlycon debugging: flip these to =y and add earlycon=uart8250,io,0x3f8,115200 to cmdline
CONFIG_SERIAL_8250=n
CONFIG_SERIAL_8250_CONSOLE=n
CONFIG_SERIAL_EARLYCON=n
# Unused filesystems (rootfs is virtiofs; ext4 kept for app/block use)
CONFIG_BTRFS_FS=n
CONFIG_XFS_FS=n
CONFIG_FAT_FS=n
CONFIG_VFAT_FS=n
# TUN not needed in guest (virtio_net used; TUN is for VPN/tap tools)
CONFIG_TUN=n
# DM-Crypt and DM-Integrity not used in microVM
CONFIG_DM_CRYPT=n
CONFIG_DM_INTEGRITY=n
# SELinux and audit not needed in microVM (~5ms savings from AUDIT alone)
CONFIG_SECURITY_SELINUX=n
CONFIG_AUDIT=n
KCONFIG_EOF
          '';
        });
      in
      {
        packages.libkrunfw-vmgenid = libkrunfw-vmgenid;

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

            # VM firmware with VMGENID support via SETUP_VMGENID boot protocol
            libkrunfw-vmgenid

            # for --features snd (virtio-snd pipewire backend)
            pipewire.dev

            # for --features gpu (virtio-gpu with virglrenderer)
            virglrenderer
            libepoxy.dev
            libdrm.dev

            # stress testing for flakiness investigation
            stress-ng

            # Task runner (replaces Makefile)
            just

            # cargo-fuzz: required for `just fuzz` and `just fuzz-all`
            cargo-fuzz

            # Cargo wrapper that dispatches +nightly/+stable and RUSTUP_TOOLCHAIN.
            # PATH position is enforced via shellHook below (setup hooks can reorder).
            cargoWrapper

            # cargo-kani proxy: bridges `cargo kani` to kani-driver in kaniBundle.
            kaniWrapper
          ];

          # Point Rust's pkg_config crate at the shim so PKG_CONFIG_PATH set by
          # cargo or run.sh is forwarded to the NixOS wrapper's target-specific var.
          PKG_CONFIG = pkgConfigShim;

          # NixOS has no /usr/include; tell clang where glibc headers live.
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include -isystem ${pkgs.pipewire.dev}/include";

          # Force gcc as the host C compiler and linker for x86_64-unknown-linux-gnu.
          # Without CC_x86_64_unknown_linux_gnu, build scripts (e.g. bzip2-sys) fall
          # back to CC which is the musl cc, causing compiler-family detection failures
          # when .cargo/config.toml sets an explicit default target.
          # The rust-overlay toolchain defaults to its bundled LLD which cannot resolve
          # glibc's open64/stat64 compat aliases on glibc 2.34+ (NixOS); gcc's ld does.
          CC_x86_64_unknown_linux_gnu = "${pkgs.gcc}/bin/gcc";
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "${pkgs.gcc}/bin/gcc";

          # Force the NixOS-wrapped clang++ for C++ compilation (used by libfuzzer-sys
          # build.rs). The gcc-wrapper g++ has a broken #include_next search order on
          # NixOS: glibc is added via -isystem (position 3) before gcc's own C++ headers
          # (position 12), so #include_next <stdlib.h> in <cstdlib> can't find glibc.
          # The NixOS clang wrapper uses a different include strategy that works correctly.
          CXX_x86_64_unknown_linux_gnu = "${cxxWrapper}/bin/clang++";

          # Linker for the x86_64-unknown-linux-musl target (guest-agent)
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER =
            "${pkgs.pkgsMusl.stdenv.cc}/bin/cc";

          shellHook = ''
            # The rust-overlay toolchain's setup hook re-adds its bin to PATH after
            # buildInputs ordering runs, overriding our cargoWrapper. Force the wrapper
            # first so `cargo +nightly` dispatch works without rustup.
            export PATH="${cargoWrapper}/bin:$PATH"

            # `make test` hardcodes LD_LIBRARY_PATH to test-prefix/lib64 only.
            # Symlink libkrunfw there so the test runner can find it alongside libkrun.
            mkdir -p test-prefix/lib64
            for lib in ${libkrunfw-vmgenid}/lib/libkrunfw*; do
              ln -sf "$lib" "$(pwd)/test-prefix/lib64/$(basename "$lib")"
            done

            # init/init.c must be statically linked; NixOS has no static glibc.
            # Use the musl toolchain instead. The Makefile uses CC_LINUX=$(CC) on Linux.
            # Set here (in shellHook, after setup hooks run) to override buildInputs CC.
            export CC="${pkgs.pkgsMusl.stdenv.cc}/bin/cc"

            # Add libclang to LD_LIBRARY_PATH so clang-sys can load it at build time
            export LD_LIBRARY_PATH="${pkgs.llvmPackages.libclang.lib}/lib:$LD_LIBRARY_PATH"

            # cargo-kani is provided by kaniWrapper in buildInputs (no install needed).
          '';
        };
      }
    );
}
