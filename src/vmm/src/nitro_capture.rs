//! Nitro adapter for the shared cooperative capture request and sealed template.
use crate::{capture_request::CaptureRequest, template::Template};
use nitro_sandbox::capture::{CaptureHandler, GuestStop, PreparedCapture};
use std::io;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub struct NitroCapture {
    pub request: CaptureRequest,
    pub vcpus: u8,
    pub ram_mib: u64,
    pub topology: Vec<u8>,
    pub compatibility: Vec<u8>,
    pub export_path: Option<std::path::PathBuf>,
}
struct Prepared {
    request: CaptureRequest,
    template: Option<Template>,
    export_path: Option<std::path::PathBuf>,
}
impl PreparedCapture for Prepared {
    fn accept(&mut self) -> bool {
        self.request.accept()
    }
    fn complete(mut self: Box<Self>) {
        let template = self.template.take().unwrap();
        let export = self
            .export_path
            .take()
            .map(|path| template.write_to_path(&path).map(|()| path));
        self.request
            .complete(Ok(crate::capture_request::CaptureOutcome {
                template,
                export,
            }));
    }
    fn reject(self: Box<Self>, stop: GuestStop) {
        // Dropping the sealed template here: it is never published.
        self.request.complete(Err(guest_stopped_error(stop)));
    }
}

/// The typed refusal for a capture whose source is ending: the stated final
/// reason when there is one, otherwise the host stop.
fn guest_stopped_error(stop: GuestStop) -> io::Error {
    let outcome = if (1..=4).contains(&stop.reason) {
        crate::vm_exit::VmExit::from_stated_reason(stop.reason)
    } else {
        crate::vm_exit::VmExit::Stopped
    };
    io::Error::other(crate::capture_request::GuestStoppedDuringCapture(outcome))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;
    #[test]
    fn shared_request_requires_arm_and_delivers_sealed_template() {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0x40000000), 16384)]).unwrap();
        let mut page = [0u8; 4096];
        for (n, value) in [
            (0, nitro_sandbox::cooperative::FROZEN_MAGIC),
            (5, 0x40001000),
            (6, 1),
            (7, 0x40002000),
            (30, 20),
            (32, 1),
        ] {
            page[n * 8..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        page[27 * 8..27 * 8 + 20].fill(7);
        memory.write_slice(&page, GuestAddress(0x40000000)).unwrap();
        let request = CaptureRequest::default();
        let mut adapter = NitroCapture {
            request: request.clone(),
            vcpus: 1,
            ram_mib: 1,
            topology: vec![],
            compatibility: vec![],
            export_path: None,
        };
        assert!(adapter.prepare(&memory, 0x40000000).unwrap().is_none());
        assert!(request.arm(std::time::Duration::from_secs(5)).is_err());
        adapter.marker_confirmed(true);
        request.arm(std::time::Duration::from_secs(5)).unwrap();
        let mut prepared = adapter.prepare(&memory, 0x40000000).unwrap().unwrap();
        assert!(prepared.accept());
        prepared.complete();
        let template = request.wait().unwrap();
        assert_eq!(template.compatibility, vec![7; 20]);
        assert!(template.file.write_all_at(&[0], 0).is_err());
    }

    fn stopped_outcome(request: &CaptureRequest) -> crate::vm_exit::VmExit {
        match request.wait() {
            Ok(_) => panic!("a template was published for an ending source"),
            Err(error) => error
                .get_ref()
                .and_then(|e| e.downcast_ref::<crate::capture_request::GuestStoppedDuringCapture>())
                .expect("typed refusal")
                .0
                .clone(),
        }
    }

    #[test]
    fn ending_source_is_refused_typed_before_and_after_acceptance() {
        // Before acceptance: a stated panic refuses the capture with the reason.
        let request = CaptureRequest::default();
        let mut adapter = NitroCapture {
            request: request.clone(),
            vcpus: 1,
            ram_mib: 1,
            topology: vec![],
            compatibility: vec![],
            export_path: None,
        };
        adapter.marker_confirmed(true);
        request.arm(std::time::Duration::from_secs(5)).unwrap();
        adapter.guest_stopped(GuestStop {
            reason: 4,
            host_stop: false,
        });
        assert_eq!(stopped_outcome(&request), crate::vm_exit::VmExit::Panic);

        // After acceptance, before publication: a host stop without a reason.
        let request = CaptureRequest::default();
        request.confirm_marker(true);
        request.arm(std::time::Duration::from_secs(5)).unwrap();
        let mut prepared: Box<dyn PreparedCapture> = Box::new(Prepared {
            request: request.clone(),
            template: None,
            export_path: None,
        });
        assert!(prepared.accept());
        prepared.reject(GuestStop {
            reason: 0,
            host_stop: true,
        });
        assert_eq!(stopped_outcome(&request), crate::vm_exit::VmExit::Stopped);
    }
}

impl CaptureHandler for NitroCapture {
    fn marker_confirmed(&mut self, active: bool) {
        self.request.confirm_marker(active);
    }
    fn prepare(
        &mut self,
        memory: &GuestMemoryMmap,
        control: u64,
    ) -> io::Result<Option<Box<dyn PreparedCapture>>> {
        if !self.request.is_armed() {
            return Ok(None);
        }
        let marker: u64 = memory
            .read_obj(GuestAddress(control + 32 * 8))
            .map_err(io::Error::other)?;
        if marker != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                crate::cooperative::TemplateMarkerMissing,
            ));
        }
        let identity_len: u64 = memory
            .read_obj(GuestAddress(control + 30 * 8))
            .map_err(io::Error::other)?;
        let mut identity = [0u8; 20];
        memory
            .read_slice(&mut identity, GuestAddress(control + 27 * 8))
            .map_err(io::Error::other)?;
        if identity_len != 20 || identity.iter().all(|byte| *byte == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "running kernel identity unavailable",
            ));
        }
        let mut compatibility = self.compatibility.clone();
        compatibility.extend_from_slice(&identity);
        if let Some(path) = std::env::var_os("NITRO_CAPTURE_PHASE_FILE") {
            std::fs::write(path, std::process::id().to_string())?;
        }
        let capture_started = std::time::Instant::now();
        let template = Template::capture_with_manifest(
            memory,
            control,
            self.vcpus,
            self.ram_mib,
            0,
            &self.topology,
            &compatibility,
        )?;
        if std::env::var_os("NITRO_COPY_TIMINGS").is_some() {
            eprintln!(
                "NITRO_CAPTURE_TIMING capture_us={} ram_mib={}",
                capture_started.elapsed().as_micros(),
                self.ram_mib
            );
        }
        Ok(Some(Box::new(Prepared {
            request: self.request.clone(),
            template: Some(template),
            export_path: self.export_path.clone(),
        })))
    }
    fn failed(&mut self, error: io::Error) {
        if self.request.is_armed() {
            self.request.complete(Err(error));
        }
    }
    fn guest_stopped(&mut self, stop: GuestStop) {
        self.failed(guest_stopped_error(stop));
    }
}
