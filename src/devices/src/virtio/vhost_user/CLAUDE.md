# Vhost-user devices

The frontend delegates virtio I/O to an external daemon. `VhostUserDevice` owns protocol negotiation, shared-memory/vring setup and interrupt monitoring; `VhostUserFs` adds filesystem config and optional DAX; `VhostUserVsock` adds guest CID config and three queues.

- Socket-path and pre-connected UnixStream construction use the same negotiation.
- Protocol feature bit 30 is stripped from guest-visible virtio features.
- MQ negotiation updates the frontend queue limit even with an explicit queue count.
- Guest memory must be memfd-backed so daemons can map it.
- `add_mem_region` requires CONFIGURE_MEM_SLOTS. DAX is an additional memfd and KVM memory slot.
- Filesystem tags are at most 36 bytes; queue layout is one HPQ plus daemon-reported request queues, size 1024.
- Vsock has RX/TX/event queues of size 256 and read-only 8-byte guest CID config. Out-of-range config reads zero the buffer.
- Reset stops interrupt monitoring and deactivates the device. Preserve monitor teardown and ordinary device reset/re-activation.

The vhost-user feature gates this module. Device activation transfers DeviceQueues through the generic frontend. State-transfer/reconnect protocols are not part of the VMM lifecycle.

Files: `device.rs` generic frontend; `fs.rs` filesystem/DAX; `vsock.rs` CID/config; `mod.rs` exports. Vendored vhost memory/queue compatibility patches are shared with integration daemons.
