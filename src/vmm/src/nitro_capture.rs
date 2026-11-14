//! Nitro adapter for the shared cooperative capture request and sealed template.
use crate::{capture_request::CaptureRequest, template::Template};
use nitro_sandbox::capture::{CaptureHandler, PreparedCapture};
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
        let template = Template::capture_with_manifest(
            memory,
            control,
            self.vcpus,
            self.ram_mib,
            0,
            &self.topology,
            &compatibility,
        )?;
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
}
