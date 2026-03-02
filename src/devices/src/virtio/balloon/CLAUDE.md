# Virtio Balloon Device

Last verified: 2026-03-02

## Purpose
Virtio memory balloon device. Allows host to reclaim guest memory (inflate) and return it (deflate). Supports stats reporting, free page hinting, and snapshot integration.

## Contracts
- **Exposes**: `Balloon` struct, `BalloonStats` struct, `TYPE_BALLOON` constant, `BalloonError` enum
- **Guarantees**:
  - 5 queues: inflate (0), deflate (1), stats (2), free page hint (3), free page reporting (4)
  - Inflate processes PFN list, calls `MADV_DONTNEED` per page; invalid PFNs silently skipped; duplicate PFNs idempotent
  - Deflate processes PFN list, calls `MADV_WILLNEED` per page
  - Config space: `num_pages` (u32, host target), `actual` (u32, guest-reported), `free_page_report_cmd_id` (u32), `poison_val` (u32)
  - Guest writes to config space `actual` field (bytes 4..8) update internal state and notify condvar
  - Stats queue: host triggers stats request via config change interrupt; guest responds with stat entries (tag u16 + val u64)
  - Free page hint queue: command ID protocol (`STOP=0`, `DONE=1`, `>=2` = active); pages only accepted when `hinting_active` and command ID matches
  - `ReclaimedBitmap`: lock-free atomic bitmap (4KB granularity) tracking inflated and reported-free pages
  - `actual_condvar()` returns `Arc<(Mutex<u64>, Condvar)>` for external await on actual field changes
  - `signal_config_change()` on `DeviceState` sends config change interrupt when device is activated; warns if inactive
  - Feature negotiation: `VIRTIO_F_VERSION_1`, `MUST_TELL_HOST`, `STATS_VQ`, `DEFLATE_ON_OOM`, `FREE_PAGE_HINT`, `PAGE_POISON`, `REPORTING`
  - Snapshot save/restore: serializes `BalloonState` (num_pages, actual, cmd_id, poison_val, hinting counter, hinting_active) via bincode behind `snapshot` feature
- **Expects**: `GuestMemoryMmap` available at activation; event manager drives queue notifications

## Dependencies
- **Uses**: `vm-memory` (GuestMemoryMmap), `virtio-queue`, `vmm-sys-util` (EventFd)
- **Used by**: `vmm` (builder attaches balloon, snapshot orchestration queries reclaimed bitmaps), `libkrun` (BalloonHandle wraps Arc<Mutex<Balloon>>)
- **Boundary**: Device does not know about snapshots directly; VMM reads bitmaps to determine excluded/reclaimed pages

## Key Decisions
- `MADV_DONTNEED` for inflate (zero-fills on next access), `MADV_WILLNEED` for deflate
- ReclaimedBitmap uses `AtomicU64` words with bit-level ops; no locks needed for mark/clear/check
- Two separate bitmaps: `inflated_bitmap` (inflate/deflate tracking) and `reported_free_bitmap` (free page hint/reporting)
- Condvar notification on `actual` update enables `BalloonHandle::await_target()` without polling

## Invariants
- Queue indices are fixed: inflate=0, deflate=1, stats=2, free_page_hint=3, free_page_reporting=4
- PFN shift is always 12 (4KB pages)
- `hinting_active` is only true between receiving a command ID >= 2 and receiving STOP/DONE
- Bitmaps are allocated at activation time based on guest memory size

## Key Files
- `device.rs` - Balloon struct, inflate/deflate/stats/PHQ processing, config space, snapshot state
- `event_handler.rs` - EventManager subscriber impl, queue event dispatch
- `reclaimed_bitmap.rs` - ReclaimedBitmap atomic bitmap implementation
- `mod.rs` - Module exports, queue/feature constants, BalloonError
