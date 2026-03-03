//! AsyncBlockBackend that returns I/O errors on configured sectors.
//!
//! Used by test_block_backend_errors.rs to verify guest and VMM behavior
//! when a block backend returns transient or permanent I/O errors.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;

use krun::{
    AsyncBlockBackend, AsyncBlockBackendFactory, BoxFuture, CacheType, SendBoxFuture,
    VolatileSliceGuard,
};

/// A block backend that returns `ErrorKind::Other` for any sector included
/// in the configured error set. All other sectors behave like MemBlockBackend.
pub struct FailingBlockBackend {
    data: Arc<tokio::sync::Mutex<Vec<u8>>>,
    sector_count: u64,
    /// Byte offsets (multiples of 512) at which reads/writes return an error.
    error_offsets: Arc<HashSet<u64>>,
}

impl FailingBlockBackend {
    /// Create a backend with `sector_count` sectors pre-filled with `fill`.
    ///
    /// `error_sectors` is a list of sector indices (0-based) that will return
    /// `io::Error` on reads and writes.
    pub fn new(
        sector_count: u64,
        fill: u8,
        error_sectors: impl IntoIterator<Item = u64>,
    ) -> (Self, Arc<tokio::sync::Mutex<Vec<u8>>>) {
        let size = (sector_count * 512) as usize;
        let data = Arc::new(tokio::sync::Mutex::new(vec![fill; size]));
        let error_offsets: HashSet<u64> = error_sectors.into_iter().map(|s| s * 512).collect();
        (
            FailingBlockBackend {
                data: data.clone(),
                sector_count,
                error_offsets: Arc::new(error_offsets),
            },
            data,
        )
    }

    fn has_error(&self, byte_offset: u64, len: u64) -> bool {
        self.error_offsets
            .iter()
            .any(|&err| err >= byte_offset && err < byte_offset + len)
    }
}

impl AsyncBlockBackend for FailingBlockBackend {
    fn cache_type(&self) -> CacheType {
        CacheType::Writeback
    }

    fn nsectors(&self) -> u64 {
        self.sector_count
    }

    fn image_id(&self) -> &[u8] {
        b"failing-block-backend"
    }

    fn read_vectored_at(
        &self,
        bufs: Vec<VolatileSliceGuard>,
        offset: u64,
    ) -> BoxFuture<'_, io::Result<usize>> {
        let total_len: usize = bufs.iter().map(|b| b.len()).sum();
        if self.has_error(offset, total_len as u64) {
            return Box::pin(async move {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("simulated read error at offset {offset}"),
                ))
            });
        }
        let data = self.data.clone();
        Box::pin(async move {
            let buf = data.lock().await;
            let mut pos = offset as usize;
            let mut total = 0usize;
            for iov in &bufs {
                let len = iov.len();
                let src = &buf[pos..pos + len];
                // SAFETY: src.len() == iov.len(); memory valid for duration of copy.
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
        let total_len: usize = bufs.iter().map(|b| b.len()).sum();
        if self.has_error(offset, total_len as u64) {
            return Box::pin(async move {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("simulated write error at offset {offset}"),
                ))
            });
        }
        let data = self.data.clone();
        Box::pin(async move {
            let mut buf = data.lock().await;
            let mut pos = offset as usize;
            let mut total = 0usize;
            for iov in &bufs {
                let len = iov.len();
                let dst = &mut buf[pos..pos + len];
                // SAFETY: dst.len() == iov.len(); memory valid for duration of copy.
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
        if self.has_error(offset, nbytes) {
            return Box::pin(async move {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("simulated write_zeroes error at offset {offset}"),
                ))
            });
        }
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

pub struct FailingBlockBackendFactory {
    sector_count: u64,
    backend: Option<FailingBlockBackend>,
}

impl FailingBlockBackendFactory {
    pub fn new(backend: FailingBlockBackend) -> Self {
        let sector_count = backend.sector_count;
        Self {
            sector_count,
            backend: Some(backend),
        }
    }
}

impl AsyncBlockBackendFactory for FailingBlockBackendFactory {
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
