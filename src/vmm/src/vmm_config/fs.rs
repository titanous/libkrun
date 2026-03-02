use devices::virtio::fs::FileSystem;

pub struct FsMount {
    pub tag: String,
    pub fs: Box<dyn FileSystem + Send + Sync>,
    pub shm_size: Option<usize>,
}
