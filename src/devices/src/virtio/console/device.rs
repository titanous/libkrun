use std::cmp;
use std::io::Write;
use std::iter::zip;
use std::mem::{size_of, size_of_val};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

use utils::eventfd::EventFd;
use vm_memory::{Address, ByteValued, Bytes, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, Queue, QueueConfig, VirtioDevice,
};
use super::{defs, defs::control_event, defs::uapi};
use crate::virtio::console::console_control::{
    ConsoleControl, VirtioConsoleControl, VirtioConsoleResize,
};
use crate::virtio::console::defs::QUEUE_SIZE;
use crate::virtio::console::port::Port;
use crate::virtio::console::port_queue_mapping::{
    num_queues, port_id_to_queue_idx, QueueDirection,
};
use crate::virtio::{InterruptTransport, PortDescription, VmmExitObserver};

pub(crate) const CONTROL_RXQ_INDEX: usize = 2;
pub(crate) const CONTROL_TXQ_INDEX: usize = 3;

pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_CONSOLE_F_SIZE as u64)
    | (1 << uapi::VIRTIO_CONSOLE_F_MULTIPORT as u64)
    | (1 << uapi::VIRTIO_F_VERSION_1 as u64);

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct VirtioConsoleConfig {
    cols: u16,
    rows: u16,
    max_nr_ports: u32,
    emerg_wr: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioConsoleConfig {}

impl VirtioConsoleConfig {
    pub fn new(cols: u16, rows: u16, max_nr_ports: u32) -> Self {
        VirtioConsoleConfig {
            cols,
            rows,
            max_nr_ports,
            emerg_wr: 0u32,
        }
    }
}

pub struct Console {
    pub(crate) device_state: DeviceState,
    pub(crate) control: Arc<ConsoleControl>,
    pub(crate) ports: Vec<Port>,

    queue_config: Vec<QueueConfig>,
    // Queues are stored as Option so individual queues can be taken when ports start.
    pub(crate) queues: Vec<Option<DeviceQueue>>,
    // TODO: move the queue event handling to the correct threads!
    pub(crate) queue_events: Vec<Arc<EventFd>>,
    /// Snapshot buffer: holds plain Queue state for save/restore via VirtioDevice trait.
    snapshot_queues: Vec<Queue>,

    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,

    pub(crate) activate_evt: EventFd,
    pub(crate) sigwinch_evt: EventFd,

    config: VirtioConsoleConfig,
}

impl Console {
    pub fn new(ports: Vec<PortDescription>) -> super::Result<Console> {
        assert!(!ports.is_empty(), "Expected at least 1 port");

        let num_queues = num_queues(ports.len());
        let queue_config: Vec<QueueConfig> = (0..num_queues)
            .map(|_| QueueConfig::new(QUEUE_SIZE))
            .collect();

        let ports: Vec<Port> = zip(0u32.., ports)
            .map(|(port_id, description)| Port::new(port_id, description))
            .collect();

        let (cols, rows) = ports[0]
            .terminal()
            .map(|t| t.get_win_size())
            .unwrap_or((0, 0));
        let config = VirtioConsoleConfig::new(cols, rows, ports.len() as u32);

        let snapshot_queues: Vec<Queue> = (0..num_queues).map(|_| Queue::new(QUEUE_SIZE)).collect();

        Ok(Console {
            control: ConsoleControl::new(),
            ports,
            queue_config,
            queues: Vec::new(),
            queue_events: Vec::new(),
            snapshot_queues,
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::ConsoleError::EventFd)?,
            sigwinch_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::ConsoleError::EventFd)?,
            device_state: DeviceState::Inactive,
            config,
        })
    }

    pub fn id(&self) -> &str {
        defs::CONSOLE_DEV_ID
    }

    pub fn get_sigwinch_fd(&self) -> RawFd {
        self.sigwinch_evt.as_raw_fd()
    }

    pub fn update_console_size(&mut self, port_id: u32, cols: u16, rows: u16) {
        log::debug!("update_console_size {port_id}: {cols} {rows}");
        self.control
            .console_resize(port_id, VirtioConsoleResize { rows, cols });
    }

    pub(crate) fn process_control_rx(&mut self) -> bool {
        log::trace!("process_control_rx called");
        let DeviceState::Activated(ref mem, _) = self.device_state else {
            unreachable!()
        };
        let mut raise_irq = false;

        let control_rx = self.queues[CONTROL_RXQ_INDEX]
            .as_mut()
            .expect("control rx queue should exist");

        log::trace!(
            "control_rx queue has buffers: {}",
            !control_rx.queue.is_empty(mem)
        );

        while let Some(head) = control_rx.queue.pop(mem) {
            if let Some(buf) = self.control.queue_pop() {
                match mem.write(&buf, head.addr) {
                    Ok(n) => {
                        if n != buf.len() {
                            log::error!("process_control_rx: partial write");
                        }
                        raise_irq = true;
                        log::trace!("process_control_rx wrote {n}");
                        if let Err(e) = control_rx.queue.add_used(mem, head.index, n as u32) {
                            error!("failed to add used elements to the queue: {e:?}");
                        }
                    }
                    Err(e) => {
                        log::error!("process_control_rx failed to write: {e}");
                    }
                }
            } else {
                control_rx.queue.undo_pop();
                break;
            }
        }
        raise_irq
    }

    pub(crate) fn process_control_tx(&mut self) -> bool {
        log::trace!("process_control_tx");
        let DeviceState::Activated(ref mem, ref interrupt) = self.device_state else {
            unreachable!()
        };

        let control_tx = self.queues[CONTROL_TXQ_INDEX]
            .as_mut()
            .expect("control tx queue should exist");
        let mut raise_irq = false;

        let mut ports_to_start = Vec::new();

        while let Some(head) = control_tx.queue.pop(mem) {
            raise_irq = true;

            let cmd: VirtioConsoleControl = match mem.read_obj(head.addr) {
                Ok(cmd) => cmd,
                Err(e) => {
                    log::error!(
                    "Failed to read VirtioConsoleControl struct: {e:?}, struct len = {len}, head.len = {head_len}",
                    len = size_of::<VirtioConsoleControl>(),
                    head_len = head.len,
                );
                    continue;
                }
            };
            if let Err(e) = control_tx
                .queue
                .add_used(mem, head.index, size_of_val(&cmd) as u32)
            {
                error!("failed to add used elements to the queue: {e:?}");
            }

            log::trace!("VirtioConsoleControl cmd: {cmd:?}");
            match cmd.event {
                control_event::VIRTIO_CONSOLE_DEVICE_READY => {
                    log::debug!(
                        "Device is ready: initialization {}",
                        if cmd.value == 1 { "ok" } else { "failed" }
                    );
                    for port_id in 0..self.ports.len() {
                        self.control.port_add(port_id as u32);
                    }
                }
                control_event::VIRTIO_CONSOLE_PORT_READY => {
                    debug!(
                        "sending PORT_READY {cmd:?} (ports: {:?})",
                        self.ports
                            .iter()
                            .map(|port| (port.port_id, port.name.clone()))
                            .collect::<Vec<_>>()
                    );

                    if cmd.value != 1 {
                        log::error!("Port initialization failed: {cmd:?}");
                        continue;
                    }

                    if let Some(term) = self.ports[cmd.id as usize].terminal() {
                        self.control.mark_console_port(mem, cmd.id);
                        self.control.port_open(cmd.id, true);
                        let (cols, rows) = term.get_win_size();
                        self.control
                            .console_resize(cmd.id, VirtioConsoleResize { cols, rows });
                    } else {
                        // We start with all ports open, this makes sense for now,
                        // because underlying file descriptors STDIN, STDOUT, STDERR are always open too
                        self.control.port_open(cmd.id, true)
                    }

                    let name = self.ports[cmd.id as usize].name();
                    log::trace!("Port ready {id}: {name}", id = cmd.id);
                    if !name.is_empty() {
                        self.control.port_name(cmd.id, name)
                    }
                }
                control_event::VIRTIO_CONSOLE_PORT_OPEN => {
                    let opened = match cmd.value {
                        0 => false,
                        1 => true,
                        _ => {
                            log::error!(
                                "Invalid value ({}) for VIRTIO_CONSOLE_PORT_OPEN on port {}",
                                cmd.value,
                                cmd.id
                            );
                            continue;
                        }
                    };

                    if !opened {
                        log::debug!("Guest closed port {}", cmd.id);
                        continue;
                    }

                    ports_to_start.push(cmd.id as usize);
                }
                _ => log::warn!("Unknown console control event {:x}", cmd.event),
            }
        }

        for port_id in ports_to_start {
            log::trace!("Starting port io for port {port_id}");
            let rx_idx = port_id_to_queue_idx(QueueDirection::Rx, port_id);
            let tx_idx = port_id_to_queue_idx(QueueDirection::Tx, port_id);

            // Take ownership of port queues - they are moved to the port.
            let rx_queue = self.queues[rx_idx]
                .take()
                .expect("port rx queue should exist")
                .queue;
            let tx_queue = self.queues[tx_idx]
                .take()
                .expect("port tx queue should exist")
                .queue;

            self.ports[port_id].start(
                mem.clone(),
                rx_queue,
                tx_queue,
                interrupt.clone(),
                self.control.clone(),
            );
        }

        raise_irq
    }

    pub(crate) fn restore_ports_after_snapshot(&mut self) {
        let (mem, interrupt) = match &self.device_state {
            DeviceState::Activated(mem, interrupt) => (mem.clone(), interrupt.clone()),
            DeviceState::Inactive => return,
        };

        for port_id in 0..self.ports.len() {
            let rx_idx = port_id_to_queue_idx(QueueDirection::Rx, port_id);
            let tx_idx = port_id_to_queue_idx(QueueDirection::Tx, port_id);
            let (Some(rx_dq), Some(tx_dq)) = (&self.queues[rx_idx], &self.queues[tx_idx]) else {
                continue;
            };
            self.ports[port_id].start(
                mem.clone(),
                rx_dq.queue.clone(),
                tx_dq.queue.clone(),
                interrupt.clone(),
                self.control.clone(),
            );
        }
    }
}

impl VirtioDevice for Console {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_CONSOLE
    }

    fn device_name(&self) -> &str {
        "console"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &self.queue_config
    }

    fn queues(&self) -> &[Queue] {
        &self.snapshot_queues
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        &mut self.snapshot_queues
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "console: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt");
            return Err(ActivateError::BadActivate);
        }

        self.queue_events = queues.iter().map(|dq| dq.event.clone()).collect();
        self.queues = queues.into_iter().map(Some).collect();

        // Populate snapshot_queues with the initial queue configuration (addresses,
        // ready flags). Port queues are later take()n by port threads, making them
        // inaccessible to sync_queues_for_snapshot(). By copying here we ensure the
        // snapshot buffer always has the correct static queue configuration.
        for (i, opt_dq) in self.queues.iter().enumerate() {
            if let Some(dq) = opt_dq {
                if i < self.snapshot_queues.len() {
                    self.snapshot_queues[i] = dq.queue.clone();
                }
            }
        }

        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn reset(&mut self) -> bool {
        // Shutdown ports and clear queues.
        for port in &mut self.ports {
            port.shutdown();
        }
        self.queues.clear();
        self.queue_events.clear();
        self.device_state = DeviceState::Inactive;
        true
    }

    fn begin_snapshot_quiesce(
        &mut self,
        _timeout: std::time::Duration,
    ) -> Result<(), crate::snapshot::SnapshotError> {
        // Shut down port threads so they stop reading/writing guest memory.
        // This is critical for restore: without it, TX threads continue to
        // read from guest RAM while load_memory overwrites it, causing stale
        // descriptors to be processed (leaking binary data to the console).
        for port in &mut self.ports {
            port.shutdown();
        }
        Ok(())
    }

    fn abort_snapshot_quiesce(&mut self) {
        // Restart port threads after a snapshot save (normal path).
        // Port threads took queues via .take() before quiesce shut them down,
        // so self.queues entries are None. Rebuild from snapshot_queues
        // (which sync_queues_for_snapshot already updated with live indices).
        for (i, opt_dq) in self.queues.iter_mut().enumerate() {
            if opt_dq.is_none() && i < self.snapshot_queues.len() && i < self.queue_events.len() {
                *opt_dq = Some(DeviceQueue::new(
                    self.snapshot_queues[i].clone(),
                    self.queue_events[i].clone(),
                ));
            }
        }
        // For restore, ports are restarted by post_restore_kick() instead.
        self.restore_ports_after_snapshot();
    }

    fn sync_queues_for_snapshot(&mut self) {
        let DeviceState::Activated(ref mem, _) = self.device_state else {
            return;
        };

        for opt_dq in &mut self.queues {
            let Some(ref mut dq) = opt_dq else {
                continue;
            };
            if !dq.queue.ready {
                continue;
            }

            let Some(avail_idx_addr) = dq.queue.avail_ring.checked_add(2) else {
                continue;
            };
            let Some(used_idx_addr) = dq.queue.used_ring.checked_add(2) else {
                continue;
            };

            let Ok(avail_idx) = mem.read_obj::<u16>(avail_idx_addr) else {
                continue;
            };
            let Ok(used_idx) = mem.read_obj::<u16>(used_idx_addr) else {
                continue;
            };

            dq.queue.set_next_avail(avail_idx);
            dq.queue.set_next_used(used_idx);
        }

        // Copy live queue state into the snapshot buffer for serialization.
        for (i, opt_dq) in self.queues.iter().enumerate() {
            if let Some(dq) = opt_dq {
                self.snapshot_queues[i] = dq.queue.clone();
            }
        }

        // For queues taken by port threads (None in self.queues), the snapshot
        // buffer already has the correct static config (addresses, ready flag)
        // from activate(). Sync the dynamic indices from guest memory.
        for (i, opt_dq) in self.queues.iter().enumerate() {
            if opt_dq.is_none() {
                let sq = &mut self.snapshot_queues[i];
                if !sq.ready {
                    continue;
                }
                let Some(avail_idx_addr) = sq.avail_ring.checked_add(2) else {
                    continue;
                };
                let Some(used_idx_addr) = sq.used_ring.checked_add(2) else {
                    continue;
                };
                if let Ok(avail_idx) = mem.read_obj::<u16>(avail_idx_addr) {
                    sq.set_next_avail(avail_idx);
                }
                if let Ok(used_idx) = mem.read_obj::<u16>(used_idx_addr) {
                    sq.set_next_used(used_idx);
                }
            }
        }
    }

    fn post_snapshot_restore(&mut self) {
        self.restore_ports_after_snapshot();
    }

    fn post_restore_kick(&mut self) {
        // During restore_state(), post_snapshot_restore() is called while the
        // device is still Inactive (activation is deferred to complete_restore),
        // so restore_ports_after_snapshot() bails out without starting any port
        // threads. By the time post_restore_kick() runs (from complete_restore),
        // the device has been activated, so we can start ports now.
        self.restore_ports_after_snapshot();

        // Kick all ready queues so the event handler can deliver any pending
        // notifications to the newly-started port threads.
        if !self.device_state.is_activated() {
            return;
        }
        for (i, evt) in self.queue_events.iter().enumerate() {
            let ready = self
                .queues
                .get(i)
                .and_then(|opt| opt.as_ref())
                .is_some_and(|dq| dq.queue.ready);
            if !ready {
                continue;
            }
            if let Err(e) = evt.write(1) {
                error!("console: post_restore_kick queue {i} failed: {e}");
            }
        }
    }
}

impl VmmExitObserver for Console {
    fn on_vmm_exit(&mut self) {
        self.reset();
        log::trace!("Console on_vmm_exit finished");
    }
}
