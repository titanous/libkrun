mod device;
mod event_handler;

pub use self::defs::uapi::VIRTIO_ID_BALLOON as TYPE_BALLOON;
pub use self::device::{Balloon, BalloonStats};

mod defs {
    use super::super::QueueConfig;

    pub const BALLOON_DEV_ID: &str = "virtio_balloon";
    pub const NUM_QUEUES: usize = 5;
    pub const QUEUE_SIZE: u16 = 256;
    pub static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [QueueConfig::new(QUEUE_SIZE); NUM_QUEUES];

    pub mod uapi {
        pub const VIRTIO_F_VERSION_1: u32 = 32;
        pub const VIRTIO_ID_BALLOON: u32 = 5;
        pub const VIRTIO_BALLOON_F_MUST_TELL_HOST: u32 = 0;
        pub const VIRTIO_BALLOON_F_STATS_VQ: u32 = 1;
        pub const VIRTIO_BALLOON_F_DEFLATE_ON_OOM: u32 = 2;
        pub const VIRTIO_BALLOON_F_PAGE_POISON: u32 = 4;
        pub const VIRTIO_BALLOON_F_FREE_PAGE_HINT: u32 = 3;
        pub const VIRTIO_BALLOON_F_REPORTING: u32 = 5;
        pub const VIRTIO_BALLOON_PFN_SHIFT: u32 = 12;

        // Stats queue tags (matching Linux spec)
        pub const VIRTIO_BALLOON_S_SWAP_IN: u16 = 0;
        pub const VIRTIO_BALLOON_S_SWAP_OUT: u16 = 1;
        pub const VIRTIO_BALLOON_S_MAJFLT: u16 = 2;
        pub const VIRTIO_BALLOON_S_MINFLT: u16 = 3;
        pub const VIRTIO_BALLOON_S_MEMFREE: u16 = 4;
        pub const VIRTIO_BALLOON_S_MEMTOT: u16 = 5;
        pub const VIRTIO_BALLOON_S_AVAIL: u16 = 6;
        pub const VIRTIO_BALLOON_S_CACHES: u16 = 7;
        pub const VIRTIO_BALLOON_S_HTLB_PGALLOC: u16 = 8;
        pub const VIRTIO_BALLOON_S_HTLB_PGFAIL: u16 = 9;
        pub const VIRTIO_BALLOON_S_OOM_KILL: u16 = 10;
        pub const VIRTIO_BALLOON_S_ALLOC_STALL: u16 = 11;
        pub const VIRTIO_BALLOON_S_ASYNC_SCAN: u16 = 12;
        pub const VIRTIO_BALLOON_S_DIRECT_SCAN: u16 = 13;
        pub const VIRTIO_BALLOON_S_ASYNC_RECLAIM: u16 = 14;
        pub const VIRTIO_BALLOON_S_DIRECT_RECLAIM: u16 = 15;
        #[allow(dead_code)]
        pub const VIRTIO_BALLOON_S_NR: u16 = 16;

        // Free page hinting command IDs
        pub const VIRTIO_BALLOON_CMD_ID_STOP: u32 = 0;
        pub const VIRTIO_BALLOON_CMD_ID_DONE: u32 = 1;
    }
}

#[derive(Debug)]
pub enum BalloonError {
    /// Failed to create event fd.
    EventFd(std::io::Error),
}

type Result<T> = std::result::Result<T, BalloonError>;
