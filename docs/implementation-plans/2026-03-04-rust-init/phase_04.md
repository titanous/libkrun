# Rust Init Migration - Phase 4: Remove C Init and Clean Up

**Goal:** Remove the C init code and update the build system. After this phase, no C init code remains.

**Architecture:** Delete `init/init.c` and `init/jsmn.h`. Remove the musl CC export from `flake.nix` (Rust handles its own musl toolchain via `.cargo/config.toml`). Verify `just all` and `just integration` pass.

**Tech Stack:** N/A (cleanup phase)

**Scope:** 4 phases from original design (phase 4 of 4)

**Codebase verified:** 2026-03-04

---

## Acceptance Criteria Coverage

This phase implements and tests:

### rust-init.AC6: C init removed, build system updated
- **rust-init.AC6.1 Success:** `init/init.c` and `init/jsmn.h` are deleted
- **rust-init.AC6.2 Success:** `just all` passes with Rust init
- **rust-init.AC6.3 Success:** `just integration` passes with Rust init

---

<!-- START_TASK_1 -->
### Task 1: Delete C init files

**Verifies:** rust-init.AC6.1

**Files:**
- Delete: `init/init.c`
- Delete: `init/jsmn.h`

**Step 1: Delete the files**

```bash
rm init/init.c init/jsmn.h
```

**Step 2: Verify no remaining references to init.c or jsmn.h**

```bash
grep -r "init\.c\|jsmn\.h" --include="*.rs" --include="*.toml" --include="*.nix" --include="justfile" .
```

Expected: No references found (the Makefile is gone, and there should be no remaining build rules that reference these files).

**Commit:** `chore(init): remove C init source files (init.c, jsmn.h)`
<!-- END_TASK_1 -->

<!-- START_TASK_2 -->
### Task 2: Remove musl CC export from flake.nix

**Files:**
- Modify: `flake.nix:307-310` (remove the musl CC export block)

**Implementation:**

Remove the following block from `flake.nix` (approximately lines 307-310 — line numbers are approximate; search for the `export CC=` block):

```nix
            # init/init.c must be statically linked; NixOS has no static glibc.
            # Use the musl toolchain instead. The Makefile uses CC_LINUX=$(CC) on Linux.
            # Set here (in shellHook, after setup hooks run) to override buildInputs CC.
            export CC="${pkgs.pkgsMusl.stdenv.cc}/bin/cc"
```

The Rust init crate handles its own musl toolchain via `init/.cargo/config.toml` (which sets `target = "x86_64-unknown-linux-musl"`) and the `rust-toolchain.toml` musl target. The CC export was only needed for the C compiler to compile `init/init.c`.

**Important:** The `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER` setting (if present elsewhere in flake.nix) should be kept — it's used by the test workspace's `guest-agent` binary, not by the init.

**Step 1: Remove the musl CC export lines**

**Step 2: Verify the nix shell still works**

```bash
exit  # leave current shell
nix develop .  # re-enter
```

Expected: Shell opens without errors.

**Step 3: Verify init still builds**

```bash
just build-init
```

Expected: Builds successfully (Rust doesn't use CC for musl).

**Commit:** `chore(nix): remove musl CC export for deleted C init`
<!-- END_TASK_2 -->

<!-- START_TASK_3 -->
### Task 3: Verify full test suite passes

**Verifies:** rust-init.AC6.2, rust-init.AC6.3

**Files:**
- No code changes — verification only

**Step 1: Run the fast suite**

```bash
just all
```

Expected: check + test + miri + proptest + loom + shuttle all pass.

**Step 2: Run integration tests**

```bash
just integration
```

Expected: All 49 tests pass.

**Step 3: Verify no C artifacts remain**

```bash
ls init/
```

Expected: Only `Cargo.toml`, `Cargo.lock`, `.cargo/`, `src/`, `target/`, `init` (the compiled binary). No `.c` or `.h` files.

**Commit:** No commit — verification only.
<!-- END_TASK_3 -->
