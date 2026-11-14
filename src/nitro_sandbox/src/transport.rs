//! Adapter from Nitro's PCI mirror to the fork's existing virtio devices.
use crate::host::{DeviceEvents, Sandbox};
use crate::{Action, DeviceLifecycle};
use devices::virtio::{DeviceQueue, InterruptTransport, Queue, VirtioDevice};
use std::io;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::sync::{Arc, Mutex};
use utils::eventfd::EventFd;
use vm_memory::{GuestAddress, GuestMemory, GuestMemoryMmap};

pub struct Transport {
    device: Arc<Mutex<dyn VirtioDevice>>,
    events: DeviceEvents,
    lifecycle: DeviceLifecycle,
    offered: u64,
}
fn event(file: &std::fs::File) -> io::Result<EventFd> {
    Ok(unsafe { EventFd::from_raw_fd(file.try_clone()?.into_raw_fd()) })
}
impl Transport {
    pub fn register(sandbox: &Sandbox, device: Arc<Mutex<dyn VirtioDevice>>) -> io::Result<Self> {
        let guard = device.lock().unwrap();
        let offered =
            (guard.avail_features() | (1 << 32)) & !((1 << 28) | (1 << 29) | (1 << 34) | (1 << 40));
        let mut config = [0u8; 64];
        let config_size = if guard.device_type() == 19 { 8 } else { 64 };
        guard.read_config(0, &mut config[..config_size]);
        let events = sandbox.add_device(
            guard.device_type(),
            guard.queue_config().len(),
            offered,
            &config[..config_size],
        )?;
        drop(guard);
        Ok(Self {
            device,
            events,
            lifecycle: DeviceLifecycle::default(),
            offered,
        })
    }
    pub fn refresh(&mut self, sandbox: &Sandbox, memory: &GuestMemoryMmap) -> io::Result<Action> {
        let Some(snapshot) = sandbox.snapshot(self.events.uid)? else {
            return Ok(Action::None);
        };
        let action = self
            .lifecycle
            .observe(&snapshot, self.offered)
            .map_err(|e| io::Error::other(format!("device lifecycle: {e:?}")))?;
        match action {
            Action::Stop => self.stop()?,
            Action::Start => {
                let mut device = self.device.lock().unwrap();
                let config = device.queue_config();
                if snapshot.queues.len() != config.len() {
                    return Err(io::Error::other("queue count mismatch"));
                }
                let mut queues = Vec::new();
                for (i, (source, maximum)) in snapshot.queues.iter().zip(config).enumerate() {
                    source
                        .validate(maximum.size, |address, length| {
                            memory.check_range(
                                GuestAddress(address),
                                length as usize,
                                vm_memory::Permissions::ReadWrite,
                            )
                        })
                        .map_err(|e| io::Error::other(format!("queue {i}: {e:?}")))?;
                    let mut q = Queue::new(maximum.size);
                    q.size = source.size;
                    q.ready = true;
                    q.desc_table = GuestAddress(source.descriptor);
                    q.avail_ring = GuestAddress(source.available);
                    q.used_ring = GuestAddress(source.used);
                    queues.push(DeviceQueue::new(q, Arc::new(event(&self.events.kicks[i])?)));
                }
                let calls = self
                    .events
                    .calls
                    .iter()
                    .map(event)
                    .collect::<io::Result<Vec<_>>>()?;
                let interrupt = InterruptTransport::new_nitro(calls, device.device_name().into())?;
                device.set_acked_features(snapshot.negotiated_features);
                device
                    .activate(memory.clone(), interrupt, queues)
                    .map_err(|e| io::Error::other(format!("activate: {e:?}")))?;
            }
            Action::None => {}
        }
        Ok(action)
    }
    pub fn stop(&mut self) -> io::Result<()> {
        let mut device = self.device.lock().unwrap();
        if device.is_activated() {
            if !device.reset() {
                return Err(io::Error::other("device refused synchronous reset"));
            }
        }
        device.set_acked_features(0);
        Ok(())
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        // A device that cannot stop must not continue accessing memory after its
        // VM is destroyed. Such a failure is fatal to this one-guest process.
        if self.stop().is_err() {
            std::process::abort();
        }
    }
}
