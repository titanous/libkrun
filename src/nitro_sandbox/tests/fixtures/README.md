# Captured Nitro device entries

Captured on the m8g from the Rust product builder reboot-watchdog run, CID 212,
using the lane-owned reference guest kernel. Each file contains the actual 2344-byte
device entry copied after generation/reset counters were stable, wrapped in a
compact one-entry TOC. TOC offsets are normalized to 64; device-entry bytes are not
synthesized. Types cover block, vsock and console. No secrets or guest payload are
included; only PCI mirror metadata and ring addresses.
