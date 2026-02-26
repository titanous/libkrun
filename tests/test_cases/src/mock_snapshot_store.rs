//! Mock SnapshotStore implementations for UFFD integration testing.
//!
//! These mock stores wrap FsSnapshotStore to control preload behavior and inject
//! artificial delays for testing demand-paging paths.

use std::io;
use std::path::Path;

use krun::snapshot_store::{BoxStream, SendBoxFuture, SnapshotStore, SnapshotStoreFactory, FsSnapshotStoreFactory};
use futures::stream::{self, StreamExt};

/// EmptyPreloadStore: preload returns empty, all pages loaded via faults
pub struct EmptyPreloadStore {
    inner: Box<dyn SnapshotStore>,
}

impl SnapshotStore for EmptyPreloadStore {
    fn read_vmstate(&self) -> SendBoxFuture<'_, io::Result<Vec<u8>>> {
        self.inner.read_vmstate()
    }

    fn read_page(&self, guest_addr: u64) -> SendBoxFuture<'_, io::Result<Vec<u8>>> {
        self.inner.read_page(guest_addr)
    }

    fn preload(&self, _regions: Vec<(u64, u64)>) -> BoxStream<'_, io::Result<(u64, Vec<u8>)>> {
        // Return empty stream: no preload, all pages via faults
        Box::pin(stream::iter(vec![]))
    }

    fn write_vmstate(&self, data: Vec<u8>) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.write_vmstate(data)
    }

    fn write_pages(&self, pages: Vec<(u64, Vec<u8>)>) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.write_pages(pages)
    }

    fn close(&self) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.close()
    }
}

/// Factory for EmptyPreloadStore
pub struct EmptyPreloadStoreFactory {
    base_path: std::path::PathBuf,
    incremental_paths: Vec<std::path::PathBuf>,
}

impl EmptyPreloadStoreFactory {
    pub fn new(base_path: impl AsRef<Path>, incremental_paths: &[impl AsRef<Path>]) -> Self {
        Self {
            base_path: base_path.as_ref().to_path_buf(),
            incremental_paths: incremental_paths.iter().map(|p| p.as_ref().to_path_buf()).collect(),
        }
    }
}

impl SnapshotStoreFactory for EmptyPreloadStoreFactory {
    fn create(self: Box<Self>) -> SendBoxFuture<'static, io::Result<Box<dyn SnapshotStore>>> {
        Box::pin(async move {
            let inner_factory = FsSnapshotStoreFactory::new(&self.base_path, &self.incremental_paths);
            let inner = Box::new(inner_factory).create().await?;
            Ok(Box::new(EmptyPreloadStore { inner }) as Box<dyn SnapshotStore>)
        })
    }
}

/// PartialPreloadStore: preload yields only first N% of memory
pub struct PartialPreloadStore {
    inner: Box<dyn SnapshotStore>,
    preload_fraction: f64,
}

impl PartialPreloadStore {
    fn new(inner: Box<dyn SnapshotStore>, preload_fraction: f64) -> Self {
        Self { inner, preload_fraction }
    }
}

impl SnapshotStore for PartialPreloadStore {
    fn read_vmstate(&self) -> SendBoxFuture<'_, io::Result<Vec<u8>>> {
        self.inner.read_vmstate()
    }

    fn read_page(&self, guest_addr: u64) -> SendBoxFuture<'_, io::Result<Vec<u8>>> {
        self.inner.read_page(guest_addr)
    }

    fn preload(&self, regions: Vec<(u64, u64)>) -> BoxStream<'_, io::Result<(u64, Vec<u8>)>> {
        let inner = self.inner.as_ref();
        let fraction = self.preload_fraction;

        // Calculate total preload size
        let total_size: u64 = regions.iter().map(|(_, size)| size).sum();
        let preload_size = ((total_size as f64) * fraction) as u64;

        // Create adjusted regions for partial preload
        let mut partial_regions = Vec::new();
        let mut accumulated = 0u64;

        for (addr, size) in regions {
            let remaining = preload_size.saturating_sub(accumulated);
            if remaining == 0 {
                break;
            }
            let take = std::cmp::min(size, remaining);
            partial_regions.push((addr, take));
            accumulated += take;
        }

        // Get stream from inner store and let it preload only partial regions
        inner.preload(partial_regions)
    }

    fn write_vmstate(&self, data: Vec<u8>) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.write_vmstate(data)
    }

    fn write_pages(&self, pages: Vec<(u64, Vec<u8>)>) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.write_pages(pages)
    }

    fn close(&self) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.close()
    }
}

/// Factory for PartialPreloadStore
pub struct PartialPreloadStoreFactory {
    base_path: std::path::PathBuf,
    incremental_paths: Vec<std::path::PathBuf>,
    preload_fraction: f64,
}

impl PartialPreloadStoreFactory {
    pub fn new(base_path: impl AsRef<Path>, incremental_paths: &[impl AsRef<Path>], preload_fraction: f64) -> Self {
        Self {
            base_path: base_path.as_ref().to_path_buf(),
            incremental_paths: incremental_paths.iter().map(|p| p.as_ref().to_path_buf()).collect(),
            preload_fraction,
        }
    }
}

impl SnapshotStoreFactory for PartialPreloadStoreFactory {
    fn create(self: Box<Self>) -> SendBoxFuture<'static, io::Result<Box<dyn SnapshotStore>>> {
        Box::pin(async move {
            let inner_factory = FsSnapshotStoreFactory::new(&self.base_path, &self.incremental_paths);
            let inner = Box::new(inner_factory).create().await?;
            Ok(Box::new(PartialPreloadStore::new(inner, self.preload_fraction)) as Box<dyn SnapshotStore>)
        })
    }
}

/// ErrorStore: read_page returns Err for a specific guest address
pub struct ErrorStore {
    inner: Box<dyn SnapshotStore>,
    error_addr: u64,
}

impl ErrorStore {
    fn new(inner: Box<dyn SnapshotStore>, error_addr: u64) -> Self {
        Self { inner, error_addr }
    }
}

impl SnapshotStore for ErrorStore {
    fn read_vmstate(&self) -> SendBoxFuture<'_, io::Result<Vec<u8>>> {
        self.inner.read_vmstate()
    }

    fn read_page(&self, guest_addr: u64) -> SendBoxFuture<'_, io::Result<Vec<u8>>> {
        if guest_addr == self.error_addr {
            Box::pin(async {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("simulated read_page failure at addr 0x{:x}", guest_addr),
                ))
            })
        } else {
            self.inner.read_page(guest_addr)
        }
    }

    fn preload(&self, regions: Vec<(u64, u64)>) -> BoxStream<'_, io::Result<(u64, Vec<u8>)>> {
        self.inner.preload(regions)
    }

    fn write_vmstate(&self, data: Vec<u8>) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.write_vmstate(data)
    }

    fn write_pages(&self, pages: Vec<(u64, Vec<u8>)>) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.write_pages(pages)
    }

    fn close(&self) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.close()
    }
}

/// Factory for ErrorStore
pub struct ErrorStoreFactory {
    base_path: std::path::PathBuf,
    incremental_paths: Vec<std::path::PathBuf>,
    error_addr: u64,
}

impl ErrorStoreFactory {
    pub fn new(base_path: impl AsRef<Path>, incremental_paths: &[impl AsRef<Path>], error_addr: u64) -> Self {
        Self {
            base_path: base_path.as_ref().to_path_buf(),
            incremental_paths: incremental_paths.iter().map(|p| p.as_ref().to_path_buf()).collect(),
            error_addr,
        }
    }
}

impl SnapshotStoreFactory for ErrorStoreFactory {
    fn create(self: Box<Self>) -> SendBoxFuture<'static, io::Result<Box<dyn SnapshotStore>>> {
        Box::pin(async move {
            let inner_factory = FsSnapshotStoreFactory::new(&self.base_path, &self.incremental_paths);
            let inner = Box::new(inner_factory).create().await?;
            Ok(Box::new(ErrorStore::new(inner, self.error_addr)) as Box<dyn SnapshotStore>)
        })
    }
}

/// DelayStore: read_page adds artificial 50ms sleep before delegating
pub struct DelayStore {
    inner: Box<dyn SnapshotStore>,
    delay_ms: u64,
}

impl DelayStore {
    fn new(inner: Box<dyn SnapshotStore>, delay_ms: u64) -> Self {
        Self { inner, delay_ms }
    }
}

impl SnapshotStore for DelayStore {
    fn read_vmstate(&self) -> SendBoxFuture<'_, io::Result<Vec<u8>>> {
        self.inner.read_vmstate()
    }

    fn read_page(&self, guest_addr: u64) -> SendBoxFuture<'_, io::Result<Vec<u8>>> {
        let inner = self.inner.as_ref();
        let delay_ms = self.delay_ms;

        Box::pin(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            inner.read_page(guest_addr).await
        })
    }

    fn preload(&self, _regions: Vec<(u64, u64)>) -> BoxStream<'_, io::Result<(u64, Vec<u8>)>> {
        // Return empty stream: no preload, all pages via read_page with delay
        Box::pin(stream::iter(vec![]))
    }

    fn write_vmstate(&self, data: Vec<u8>) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.write_vmstate(data)
    }

    fn write_pages(&self, pages: Vec<(u64, Vec<u8>)>) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.write_pages(pages)
    }

    fn close(&self) -> SendBoxFuture<'_, io::Result<()>> {
        self.inner.close()
    }
}

/// Factory for DelayStore
pub struct DelayStoreFactory {
    base_path: std::path::PathBuf,
    incremental_paths: Vec<std::path::PathBuf>,
    delay_ms: u64,
}

impl DelayStoreFactory {
    pub fn new(base_path: impl AsRef<Path>, incremental_paths: &[impl AsRef<Path>], delay_ms: u64) -> Self {
        Self {
            base_path: base_path.as_ref().to_path_buf(),
            incremental_paths: incremental_paths.iter().map(|p| p.as_ref().to_path_buf()).collect(),
            delay_ms,
        }
    }
}

impl SnapshotStoreFactory for DelayStoreFactory {
    fn create(self: Box<Self>) -> SendBoxFuture<'static, io::Result<Box<dyn SnapshotStore>>> {
        Box::pin(async move {
            let inner_factory = FsSnapshotStoreFactory::new(&self.base_path, &self.incremental_paths);
            let inner = Box::new(inner_factory).create().await?;
            Ok(Box::new(DelayStore::new(inner, self.delay_ms)) as Box<dyn SnapshotStore>)
        })
    }
}
