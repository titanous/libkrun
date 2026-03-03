//! AsyncBlockBackend with configurable per-operation delay.
//!
//! Used by test_block_backend_slow.rs to exercise VMM behavior under
//! a block backend with artificial latency.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use krun::{
    AsyncBlockBackend, AsyncBlockBackendFactory, BoxFuture, CacheType, SendBoxFuture,
    VolatileSliceGuard,
};

/// A block backend that inserts a fixed delay before every read or write,
/// then delegates to an in-memory buffer.
pub struct SlowBlockBackend {
    data: Arc<tokio::sync::Mutex<Vec<u8>>>,
    sector_count: u64,
    delay: Duration,
}

impl SlowBlockBackend {
    /// Create a backend with `sector_count` sectors pre-filled with `fill` and
    /// a `delay` applied before every read or write operation.
    pub fn new(
        sector_count: u64,
        fill: u8,
        delay: Duration,
    ) -> (Self, Arc<tokio::sync::Mutex<Vec<u8>>>) {
        let size = (sector_count * 512) as usize;
        let data = Arc::new(tokio::sync::Mutex::new(vec![fill; size]));
        (
            SlowBlockBackend {
                data: data.clone(),
                sector_count,
                delay,
            },
            data,
        )
    }
}

impl AsyncBlockBackend for SlowBlockBackend {
    fn cache_type(&self) -> CacheType {
        CacheType::Writeback
    }

    fn nsectors(&self) -> u64 {
        self.sector_count
    }

    fn image_id(&self) -> &[u8] {
        b"slow-block-backend"
    }

    fn read_vectored_at(
        &self,
        bufs: Vec<VolatileSliceGuard>,
        offset: u64,
    ) -> BoxFuture<'_, io::Result<usize>> {
        let data = self.data.clone();
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
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
        let data = self.data.clone();
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
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
        let data = self.data.clone();
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            let mut buf = data.lock().await;
            let start = offset as usize;
            let end = start + nbytes as usize;
            buf[start..end].fill(0);
            Ok(())
        })
    }
}

pub struct SlowBlockBackendFactory {
    sector_count: u64,
    backend: Option<SlowBlockBackend>,
}

impl SlowBlockBackendFactory {
    pub fn new(backend: SlowBlockBackend) -> Self {
        let sector_count = backend.sector_count;
        Self {
            sector_count,
            backend: Some(backend),
        }
    }
}

impl AsyncBlockBackendFactory for SlowBlockBackendFactory {
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
