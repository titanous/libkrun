# Virtio balloon device

The balloon supports inflation/deflation, statistics, free-page hinting and reporting. Five queue indices are fixed: inflate=0, deflate=1, stats=2, hint=3, reporting=4.

Inflation uses MADV_DONTNEED; deflation uses MADV_WILLNEED. Invalid PFNs are skipped. Config `actual` updates notify the condvar used by the public balloon await API. Hint commands use STOP=0, DONE=1, active IDs >=2 and accept pages only for the active matching command.

`ReclaimedBitmap` tracks inflated and reported-free pages with atomic words at 4 KiB granularity. The two bitmaps are allocated at activation and used by normal page processing; retain their property/concurrency tests. Config-change interrupts require an activated device.

Files: `device.rs` owns queue/config processing and stats; `event_handler.rs` dispatches events; `reclaimed_bitmap.rs` owns atomic tracking; `mod.rs` defines constants/errors/exports. The VMM attaches the device and libkrun exposes BalloonHandle.
