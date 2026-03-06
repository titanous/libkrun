#![no_main]

use std::fs::File;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;
use vhost::vhost_user::message::{
    VhostUserConfigFlags, VhostUserInflight, VhostUserLog, VhostUserMemoryRegion,
    VhostUserProtocolFeatures, VhostUserSharedMsg, VhostUserSingleMemoryRegion,
    VhostUserVringAddrFlags, VhostUserVringState, VhostTransferStateDirection,
    VhostTransferStatePhase,
};
use vhost::vhost_user::{BackendReqHandler, GpuBackend, VhostUserBackendReqHandlerMut};

type Result<T> = std::result::Result<T, vhost::vhost_user::Error>;

/// A minimal backend: returns empty/zero responses for capability queries and
/// `InvalidParam` for operations requiring real resources (fds, memory regions).
/// Any of these responses are valid from the protocol's perspective — the fuzzer
/// drives the frontend side, so the backend result doesn't matter; what matters
/// is that handle_request() reaches the message dispatch without panicking.
struct NullBackend;

impl VhostUserBackendReqHandlerMut for NullBackend {
    fn set_owner(&mut self) -> Result<()> {
        Ok(())
    }
    fn reset_owner(&mut self) -> Result<()> {
        Ok(())
    }
    fn reset_device(&mut self) -> Result<()> {
        Ok(())
    }
    fn get_features(&mut self) -> Result<u64> {
        Ok(0)
    }
    fn set_features(&mut self, _: u64) -> Result<()> {
        Ok(())
    }
    fn set_mem_table(&mut self, _: &[VhostUserMemoryRegion], _: Vec<File>) -> Result<()> {
        Ok(())
    }
    fn set_vring_num(&mut self, _: u32, _: u32) -> Result<()> {
        Ok(())
    }
    fn set_vring_addr(
        &mut self,
        _: u32,
        _: VhostUserVringAddrFlags,
        _: u64,
        _: u64,
        _: u64,
        _: u64,
    ) -> Result<()> {
        Ok(())
    }
    fn set_vring_base(&mut self, _: u32, _: u32) -> Result<()> {
        Ok(())
    }
    fn get_vring_base(&mut self, index: u32) -> Result<VhostUserVringState> {
        Ok(VhostUserVringState::new(index, 0))
    }
    fn set_vring_kick(&mut self, _: u8, _: Option<File>) -> Result<()> {
        Ok(())
    }
    fn set_vring_call(&mut self, _: u8, _: Option<File>) -> Result<()> {
        Ok(())
    }
    fn set_vring_err(&mut self, _: u8, _: Option<File>) -> Result<()> {
        Ok(())
    }
    fn get_protocol_features(&mut self) -> Result<VhostUserProtocolFeatures> {
        Ok(VhostUserProtocolFeatures::empty())
    }
    fn set_protocol_features(&mut self, _: u64) -> Result<()> {
        Ok(())
    }
    fn get_queue_num(&mut self) -> Result<u64> {
        Ok(1)
    }
    fn set_vring_enable(&mut self, _: u32, _: bool) -> Result<()> {
        Ok(())
    }
    fn get_config(&mut self, _: u32, _: u32, _: VhostUserConfigFlags) -> Result<Vec<u8>> {
        Ok(vec![])
    }
    fn set_config(&mut self, _: u32, _: &[u8], _: VhostUserConfigFlags) -> Result<()> {
        Ok(())
    }
    fn set_gpu_socket(&mut self, _: GpuBackend) -> Result<()> {
        Err(vhost::vhost_user::Error::InvalidParam)
    }
    fn get_shared_object(&mut self, _: VhostUserSharedMsg) -> Result<File> {
        Err(vhost::vhost_user::Error::InvalidParam)
    }
    fn get_inflight_fd(&mut self, _: &VhostUserInflight) -> Result<(VhostUserInflight, File)> {
        Err(vhost::vhost_user::Error::InvalidParam)
    }
    fn set_inflight_fd(&mut self, _: &VhostUserInflight, _: File) -> Result<()> {
        Err(vhost::vhost_user::Error::InvalidParam)
    }
    fn get_max_mem_slots(&mut self) -> Result<u64> {
        Ok(0)
    }
    fn add_mem_region(&mut self, _: &VhostUserSingleMemoryRegion, _: File) -> Result<()> {
        Err(vhost::vhost_user::Error::InvalidParam)
    }
    fn remove_mem_region(&mut self, _: &VhostUserSingleMemoryRegion) -> Result<()> {
        Err(vhost::vhost_user::Error::InvalidParam)
    }
    fn set_device_state_fd(
        &mut self,
        _: VhostTransferStateDirection,
        _: VhostTransferStatePhase,
        _: File,
    ) -> Result<Option<File>> {
        Err(vhost::vhost_user::Error::InvalidParam)
    }
    fn check_device_state(&mut self) -> Result<()> {
        Err(vhost::vhost_user::Error::InvalidParam)
    }
    fn set_log_base(&mut self, _: &VhostUserLog, _: File) -> Result<()> {
        Err(vhost::vhost_user::Error::InvalidParam)
    }
}

fuzz_target!(|data: &[u8]| {
    // Create a real Unix socket pair: one end for injecting fuzz bytes as a
    // vhost-user frontend message, other end handed to BackendReqHandler.
    let (mut tx, rx) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(_) => return,
    };

    // Write the full fuzz payload then drop the writer so the reader sees EOF
    // after consuming the message. handle_request() will return an Err on EOF
    // or malformed data — that's expected and fine.
    let _ = tx.write_all(data);
    drop(tx);

    // Run the real vhost-user message parser + dispatcher.
    // handle_request(): recv_header → validate → recv_body → dispatch to backend.
    // Panics are bugs; Err results are expected for arbitrary input.
    let backend = Arc::new(Mutex::new(NullBackend));
    let mut handler = BackendReqHandler::from_stream(rx, backend);
    let _ = handler.handle_request();
});
