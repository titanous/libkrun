# Generic Virtiofs — Test Requirements

Generated from: docs/design-plans/2026-03-01-generic-virtiofs.md

---

## Automated Tests

| AC | Criterion | Test Type | Phase | Test Location | Description |
|----|-----------|-----------|-------|---------------|-------------|
| generic-virtiofs.AC1.1 | `map_file` maps a file region into the DAX window at the specified offset | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | End-to-end file read through DAX-mapped PassthroughFs exercises `map_file` via FUSE_SETUPMAPPING dispatch |
| generic-virtiofs.AC1.2 | `map_data` maps anonymous memory with provided data at the specified offset | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | init binary loading uses `map_data` path in PassthroughFs; existing integration tests exercise this when the VM boots (init_inode mapping) |
| generic-virtiofs.AC1.3 | `unmap` replaces a DAX range with inaccessible pages | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | Guest file I/O triggers FUSE_REMOVEMAPPING which calls `mapper.unmap()` through the generic path |
| generic-virtiofs.AC1.4 | `map_file` rejects mapping that would exceed DAX window bounds | unit | Phase 1 | `src/devices/src/virtio/fs/dax_mapper.rs` (`#[cfg(test)] mod tests`) | Unit test constructs `LinuxDaxMapper` with known size and verifies `map_file` with `dax_offset + len > size` returns `EINVAL`. Tests boundary, overflow, and zero-size window cases. |
| generic-virtiofs.AC1.5 | `unmap` rejects range that would exceed DAX window bounds | unit | Phase 1 | `src/devices/src/virtio/fs/dax_mapper.rs` (`#[cfg(test)] mod tests`) | Unit test constructs `LinuxDaxMapper` with known size and verifies `unmap` with `dax_offset + len > size` returns `EINVAL`. Same boundary patterns as AC1.4. |
| generic-virtiofs.AC2.1 | `Fs::new()` accepts `Box<dyn FileSystem>` and stores it | compilation-check | Phase 4 | N/A (`cargo check -p devices`) | `Fs::new(tag, Box::new(PassthroughFs::new(cfg)?), exit_code)` compiles. Verified by `cargo check -p devices`. |
| generic-virtiofs.AC2.2 | `Fs::activate()` transfers backend ownership to FsWorker via `Option::take()` | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | The new integration test exercises the full activate path: Builder constructs `Fs` with boxed backend, `activate()` takes it via `Option::take()`, FsWorker receives and operates the backend. |
| generic-virtiofs.AC3.1 | External crate can import `FileSystem` trait from `devices` crate | compilation-check | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | Integration test imports `krun::passthrough` and `krun::FileSystem` (re-exported from `devices` crate). Compilation of the test workspace proves external import works. |
| generic-virtiofs.AC3.2 | All types referenced in FileSystem method signatures are also exported | compilation-check | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | Integration test constructs `passthrough::Config` and `PassthroughFs`, uses `FileSystem` trait. If any referenced types were missing from exports, the test workspace would fail to compile. |
| generic-virtiofs.AC4.1 | `Box<dyn FileSystem>` compiles | compilation-check | Phase 2 | N/A (`cargo check -p devices`) | After Phase 2 Task 6, `cargo check -p devices` confirms `Box<dyn FileSystem>` is accepted. Phase 6 integration test also creates `Box::new(pt)` where `pt: PassthroughFs`. |
| generic-virtiofs.AC4.2 | `setupmapping` and `removemapping` accept `&dyn DaxMapper` | compilation-check | Phase 2 | N/A (`cargo check -p devices`) | Trait definition changes `setupmapping`/`removemapping` signatures to take `mapper: &dyn DaxMapper`. Server dispatch creates `LinuxDaxMapper` and passes `&mapper`. Compilation proves object-safe dispatch. |
| generic-virtiofs.AC4.3 | `Inode` and `Handle` are shared newtypes preventing mix-ups at the type level | compilation-check | Phase 2 | N/A (`cargo check -p devices`) | `Inode(u64)` and `Handle(u64)` newtypes replace associated types. Compiler enforces type-level separation — passing a raw `u64` where `Inode` is expected is a type error. |
| generic-virtiofs.AC5.1 | PassthroughFs `setupmapping` calls `mapper.map_file()` for regular files | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | Guest reads `/test-data.txt` through DAX path, triggering FUSE_SETUPMAPPING which calls `mapper.map_file()` on the regular file. |
| generic-virtiofs.AC5.2 | PassthroughFs `setupmapping` calls `mapper.map_data()` for init_inode | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | VM boot loads the embedded init binary via the init_inode path, which calls `mapper.map_data()`. All integration tests that boot a VM exercise this. |
| generic-virtiofs.AC5.3 | PassthroughFs `removemapping` calls `mapper.unmap()` for each request | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | Guest file I/O triggers FUSE_REMOVEMAPPING, which iterates `requests` and calls `mapper.unmap()` for each. |
| generic-virtiofs.AC5.4 | PassthroughFs no longer contains any direct `libc::mmap` calls for DAX | compilation-check | Phase 2 | N/A (code inspection / `grep`) | After Phase 2 Task 3 rewrites `setupmapping`/`removemapping`, verify with `grep -n 'libc::mmap' src/devices/src/virtio/fs/linux/passthrough.rs` that no DAX-related `mmap` calls remain. |
| generic-virtiofs.AC6.1 | `Server` is no longer generic (`Server` not `Server<F>`) | compilation-check | Phase 4 | N/A (`cargo check -p devices`) | Phase 4 Task 1 changes `Server<F: FileSystem + Sync>` to `Server` holding `Box<dyn FileSystem + Send + Sync>`. Compilation proves the struct is non-generic. |
| generic-virtiofs.AC6.2 | FUSE_SETUPMAPPING dispatch creates `LinuxDaxMapper` from `VirtioShmRegion` and passes to FileSystem | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | Integration test with `shm_size: Some(1 << 29)` allocates a DAX window. Guest file access triggers FUSE_SETUPMAPPING, which creates `LinuxDaxMapper` and passes `&mapper` to `self.fs.setupmapping()`. |
| generic-virtiofs.AC7.1 | `Builder::add_virtiofs(tag, Box<dyn FileSystem>, Option<usize>)` registers a filesystem mount | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | Test calls `builder.add_virtiofs("/dev/root", Box::new(pt), Some(1 << 29))` and boots a VM. Success proves the API registers the mount correctly. |
| generic-virtiofs.AC7.2 | DAX window is allocated via ShmManager when `shm_size` is `Some` | integration | Phase 6 | `tests/test_cases/src/test_virtiofs_generic_passthrough.rs` | Test passes `Some(1 << 29)` as shm_size. `create_guest_memory` allocates the SHM region via `shm_manager.create_fs_region()`. Guest DAX file access succeeds only if the window was allocated. |
| generic-virtiofs.AC7.3 | No DAX window allocated when `shm_size` is `None` | compilation-check | Phase 5 | N/A (`cargo check -p vmm`) | The `create_guest_memory` loop skips `create_fs_region` when `mount.shm_size` is `None`. Verified by code structure; no dedicated test. |
| generic-virtiofs.AC7.4 | Multiple `add_virtiofs()` calls create independent devices with separate DAX windows | integration | Phase 6 | Existing integration tests | Existing tests that combine `set_root` (in-process virtiofs) with vhost-user-fs already create multiple independent devices. The `attach_fs_devices` loop creates one `Fs` per `FsMount` entry. |
| generic-virtiofs.AC8.1 | `make test FEATURE_FLAGS="--features embedded_init"` passes with virtiofs tests exercising the new generic path | integration | Phase 6 | All tests in `tests/test_cases/src/` | Full integration test suite run. Existing tests exercise PassthroughFs through the refactored generic path (via `set_root` -> `add_virtiofs_path`). New `virtiofs-generic-passthrough` test exercises `Builder::add_virtiofs()` directly. |

## Human Verification

| AC | Criterion | Justification | Verification Approach |
|----|-----------|---------------|----------------------|
| generic-virtiofs.AC5.4 | PassthroughFs no longer contains any direct `libc::mmap` calls for DAX | Distinguishing DAX-related mmap from other potential mmap usage requires human judgment about code context. | Run `grep -n 'libc::mmap' src/devices/src/virtio/fs/linux/passthrough.rs` after Phase 2 Task 3. Verify zero matches, or if matches exist, confirm they are not in `setupmapping`/`removemapping` methods. The DAX mapping logic should exclusively use `mapper.map_file()`, `mapper.map_data()`, and `mapper.unmap()`. |
| generic-virtiofs.AC7.3 | No DAX window allocated when `shm_size` is `None` | No existing test passes `shm_size: None` for an in-process virtiofs device. Adding such a test would require a `FileSystem` impl that works without DAX, which is out of scope. | Inspect `src/vmm/src/builder.rs` in `create_guest_memory`: confirm the `if let Some(shm_size) = mount.shm_size` guard correctly skips `create_fs_region` when `None`. Trace through `attach_fs_devices` to confirm `set_shm_region` is only called when `shm_manager.fs_region(i)` returns `Some`. |

---

## Notes

1. **Phase 1 unit tests** (AC1.4, AC1.5) verify bounds-checking error paths only. They use `host_addr: 0` and trigger the bounds check before any `mmap` syscall executes, so no real memory mapping occurs. The success paths (AC1.1, AC1.2, AC1.3) are verified indirectly through integration tests that exercise the full DAX pipeline.

2. **Compilation checks** (AC2.1, AC3.1, AC3.2, AC4.1, AC4.2, AC4.3, AC6.1) are verified by `cargo check` succeeding. These criteria are structural — they assert that certain type signatures compile. The Rust type system enforces them at build time. No runtime test is needed or possible.

3. **Phase 3** (macOS code removal) has no dedicated acceptance criteria. Its correctness is verified by the absence of `#[cfg(target_os = "macos")]` in `src/devices/src/virtio/fs/` and by the build succeeding on Linux.

4. **Test flakiness**: Per project conventions, the integration test suite is inherently flaky due to VM and network timing. 5-6 out of 6 tests passing is considered normal. The new `virtiofs-generic-passthrough` test does only basic file I/O and should be stable.

5. **AC7.4** (multiple independent devices) is partially covered by existing test configurations that combine in-process virtiofs with vhost-user-fs. A dedicated test creating two in-process virtiofs devices with separate DAX windows is not included in the implementation plan but could be added as a follow-up.

6. **Indirect verification**: AC5.1, AC5.2, AC5.3, and AC6.2 cannot be directly asserted in tests (the `DaxMapper` calls happen inside opaque FUSE dispatch). They are verified indirectly: if `map_file`/`map_data`/`unmap` were not called correctly, guest file I/O would fail, and the integration test would error.
