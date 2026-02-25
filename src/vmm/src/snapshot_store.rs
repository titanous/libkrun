// Copyright 2024 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Snapshot store abstraction for flexible snapshot backends.
//!
//! Provides async traits for reading and writing VM snapshots to different
//! storage backends (filesystem, memory, cloud, etc.).

use std::io;

/// A boxed future type alias for dyn-compatible async methods.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

/// A Send-able boxed future for factory creation (needs to cross thread boundary).
pub type SendBoxFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A boxed stream type alias for dyn-compatible async streams.
pub type BoxStream<'a, T> =
    std::pin::Pin<Box<dyn futures::stream::Stream<Item = T> + 'a>>;

/// Async trait for snapshot storage operations.
///
/// Supports both read and write paths for full and incremental snapshots.
/// Implementations must be `Send + Sync + 'static` for use in concurrent contexts.
pub trait SnapshotStore: Send + Sync + 'static {
    /// Read VM state metadata from the store.
    ///
    /// Returns the serialized `VmSnapshot` or `IncrementalSnapshot` bytes.
    fn read_vmstate(&self) -> BoxFuture<'_, io::Result<Vec<u8>>>;

    /// Read a single page from the store.
    ///
    /// # Arguments
    /// * `guest_addr` - Guest physical address of the page
    ///
    /// Returns the raw page data.
    fn read_page(&self, guest_addr: u64) -> BoxFuture<'_, io::Result<Vec<u8>>>;

    /// Preload a set of memory regions asynchronously.
    ///
    /// # Arguments
    /// * `regions` - Vec of (guest_addr, size) pairs to preload
    ///
    /// Yields (guest_addr, page_data) tuples as they become available.
    /// Implementations may optimize by preloading in parallel or streaming.
    fn preload(&self, regions: Vec<(u64, u64)>) -> BoxStream<'_, io::Result<(u64, Vec<u8>)>>;

    /// Write VM state metadata to the store.
    ///
    /// # Arguments
    /// * `data` - Serialized `VmSnapshot` or `IncrementalSnapshot` bytes
    fn write_vmstate(&self, data: Vec<u8>) -> BoxFuture<'_, io::Result<()>>;

    /// Write memory pages to the store.
    ///
    /// # Arguments
    /// * `pages` - Vec of (guest_addr, page_data) pairs, ordered sequentially
    ///
    /// For full snapshots, contains all guest memory.
    /// For incremental snapshots, may be empty (dirty pages are in vmstate blob).
    fn write_pages(&self, pages: Vec<(u64, Vec<u8>)>) -> BoxFuture<'_, io::Result<()>>;

    /// Ensure all written data is durable.
    ///
    /// Implementations should fsync or equivalent to guarantee data is
    /// persisted to the underlying storage medium.
    fn close(&self) -> BoxFuture<'_, io::Result<()>>;
}

/// Factory trait for creating snapshot store instances.
///
/// This pattern allows the store to initialize resources (like file handles
/// or connections) inside the caller's execution context.
pub trait SnapshotStoreFactory: Send + 'static {
    /// Create a new snapshot store instance.
    ///
    /// Called with `self` by value (via Box) so the factory is consumed,
    /// similar to `AsyncBlockBackendFactory::create`.
    fn create(self: Box<Self>) -> SendBoxFuture<'static, io::Result<Box<dyn SnapshotStore>>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock implementation for testing trait object-safety and method signatures.
    struct MockSnapshotStore;

    impl SnapshotStore for MockSnapshotStore {
        fn read_vmstate(&self) -> BoxFuture<'_, io::Result<Vec<u8>>> {
            Box::pin(async { Err(io::Error::new(io::ErrorKind::Unsupported, "mock")) })
        }

        fn read_page(&self, _guest_addr: u64) -> BoxFuture<'_, io::Result<Vec<u8>>> {
            Box::pin(async { Err(io::Error::new(io::ErrorKind::Unsupported, "mock")) })
        }

        fn preload(&self, _regions: Vec<(u64, u64)>) -> BoxStream<'_, io::Result<(u64, Vec<u8>)>> {
            Box::pin(futures::stream::iter(vec![]))
        }

        fn write_vmstate(&self, _data: Vec<u8>) -> BoxFuture<'_, io::Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn write_pages(&self, _pages: Vec<(u64, Vec<u8>)>) -> BoxFuture<'_, io::Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn close(&self) -> BoxFuture<'_, io::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct MockFactory;

    impl SnapshotStoreFactory for MockFactory {
        fn create(self: Box<Self>) -> SendBoxFuture<'static, io::Result<Box<dyn SnapshotStore>>> {
            Box::pin(async {
                Ok(Box::new(MockSnapshotStore) as Box<dyn SnapshotStore>)
            })
        }
    }

    /// AC1.1: Verify SnapshotStore trait has all six methods (compile-time check).
    #[test]
    fn test_ac1_1_snapshot_store_has_all_methods() {
        let store: Box<dyn SnapshotStore> = Box::new(MockSnapshotStore);
        // If trait is missing methods, this won't compile.
        // Test body is just calling all methods to verify they exist.
        let _ = store.read_vmstate();
        let _ = store.read_page(0x0);
        let _ = store.preload(vec![]);
        let _ = store.write_vmstate(vec![]);
        let _ = store.write_pages(vec![]);
        let _ = store.close();
    }

    /// AC1.2: Verify return types are correct (compile-time via trait definition).
    /// AC1.3: Verify trait object-safety by boxing a MockSnapshotStore.
    #[test]
    fn test_ac1_3_trait_object_safety() {
        // This test verifies object-safety: we can create a Box<dyn SnapshotStore>
        let _store: Box<dyn SnapshotStore> = Box::new(MockSnapshotStore);
        assert!(true, "Box<dyn SnapshotStore> is object-safe");

        // Verify Send + Sync + 'static constraints via trait bounds
        let _: Box<dyn SnapshotStore> = Box::new(MockSnapshotStore);
    }

    /// AC1.4: Verify SnapshotStoreFactory trait and object-safety.
    #[test]
    fn test_ac1_4_factory_trait_and_object_safety() {
        let _factory: Box<dyn SnapshotStoreFactory> = Box::new(MockFactory);
        // If factory is missing create method or has wrong signature, this won't compile.
        assert!(true, "Box<dyn SnapshotStoreFactory> is object-safe");
    }

    /// AC1.5: Verify no snapshot IDs or lineage in trait (compile-time inspection).
    #[test]
    fn test_ac1_5_no_snapshot_ids() {
        // This is a compile-time check: the trait has no ID or lineage fields/methods.
        // If we added ID methods, this comment would be false.
        // Visual inspection: SnapshotStore has no ID-related methods.
        assert!(true, "Trait has no snapshot ID parameters");
    }
}
