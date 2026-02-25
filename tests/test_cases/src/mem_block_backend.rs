//! In-memory AsyncBlockBackend for integration tests.
//!
//! Data is stored in a shared Arc<Mutex<Vec<u8>>> so the host can inspect
//! what the guest read/wrote after the VM exits.

use std::io;
use std::sync::Arc;

use krun::{
    AsyncBlockBackend, AsyncBlockBackendFactory, BoxFuture, CacheType, SendBoxFuture,
    VolatileSliceGuard,
};

pub struct MemBlockBackend {
    data: Arc<tokio::sync::Mutex<Vec<u8>>>,
    sector_count: u64,
}

impl MemBlockBackend {
    /// Create a backend with `sector_count` sectors (each 512 bytes), pre-filled with `fill`.
    /// Returns the backend and a handle to inspect the data after the VM exits.
    pub fn new(sector_count: u64, fill: u8) -> (Self, Arc<tokio::sync::Mutex<Vec<u8>>>) {
        let size = (sector_count * 512) as usize;
        let data = Arc::new(tokio::sync::Mutex::new(vec![fill; size]));
        (
            MemBlockBackend {
                data: data.clone(),
                sector_count,
            },
            data,
        )
    }
}

impl AsyncBlockBackend for MemBlockBackend {
    fn cache_type(&self) -> CacheType {
        CacheType::Writeback
    }

    fn nsectors(&self) -> u64 {
        self.sector_count
    }

    fn image_id(&self) -> &[u8] {
        b"mem-block-backend"
    }

    fn read_vectored_at(
        &self,
        bufs: Vec<VolatileSliceGuard>,
        offset: u64,
    ) -> BoxFuture<'_, io::Result<usize>> {
        let data = self.data.clone();
        Box::pin(async move {
            let buf = data.lock().await;
            let mut pos = offset as usize;
            let mut total = 0usize;
            for iov in &bufs {
                let len = iov.len();
                let src = &buf[pos..pos + len];
                // SAFETY: `src` length == `iov.len()`; memory valid for duration of copy.
                unsafe { iov.copy_from(src) };
                pos += len;
                total += len;
            }
            Ok(total)
        })
    }

    fn write_vectored_at(
        &self,
        bufs: Vec<VolatileSliceGuard>,
        offset: u64,
    ) -> BoxFuture<'_, io::Result<usize>> {
        let data = self.data.clone();
        Box::pin(async move {
            let mut buf = data.lock().await;
            let mut pos = offset as usize;
            let mut total = 0usize;
            for iov in &bufs {
                let len = iov.len();
                let dst = &mut buf[pos..pos + len];
                // SAFETY: `dst` length == `iov.len()`; memory valid for duration of copy.
                unsafe { iov.copy_to(dst) };
                pos += len;
                total += len;
            }
            Ok(total)
        })
    }

    fn flush(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn discard(&self, _offset: u64, _nbytes: u64) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn write_zeroes(
        &self,
        offset: u64,
        nbytes: u64,
        _unmap: bool,
    ) -> BoxFuture<'_, io::Result<()>> {
        let data = self.data.clone();
        Box::pin(async move {
            let mut buf = data.lock().await;
            let start = offset as usize;
            let end = start + nbytes as usize;
            buf[start..end].fill(0);
            Ok(())
        })
    }
}

pub struct MemBlockBackendFactory {
    sector_count: u64,
    backend: Option<MemBlockBackend>,
}

impl MemBlockBackendFactory {
    pub fn new(backend: MemBlockBackend) -> Self {
        let sector_count = backend.sector_count;
        Self {
            sector_count,
            backend: Some(backend),
        }
    }
}

impl AsyncBlockBackendFactory for MemBlockBackendFactory {
    fn nsectors(&self) -> u64 {
        self.sector_count
    }

    fn cache_type(&self) -> CacheType {
        CacheType::Writeback
    }

    fn create(
        mut self: Box<Self>,
    ) -> SendBoxFuture<'static, io::Result<Arc<dyn AsyncBlockBackend + Send + Sync>>> {
        let backend = self.backend.take().expect("Factory already consumed");
        Box::pin(async move { Ok(Arc::new(backend) as Arc<dyn AsyncBlockBackend + Send + Sync>) })
    }
}
