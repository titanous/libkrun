use std::collections::HashMap;

#[derive(Clone)]
pub struct Inode {
    pub nodeid: u64,
    pub name: String,
    pub mode: u32,      // S_IFREG | 0o644
    pub size: u64,
    pub nlink: u32,
}

pub struct SyntheticFs {
    /// Fixed inode table
    pub inodes: HashMap<u64, Inode>,
    /// File content (for FUSE_READ, NOT for DAX)
    pub file_data: HashMap<u64, Vec<u8>>,
    /// DAX byte pattern (different from file_data)
    pub dax_pattern: u8,
    /// File content as seen through DAX (updated when guest writes to DAX window)
    pub dax_file_data: HashMap<u64, Vec<u8>>,
}

impl SyntheticFs {
    pub fn new() -> Self {
        let mut fs = SyntheticFs {
            inodes: HashMap::new(),
            file_data: HashMap::new(),
            dax_pattern: 0xBB,
            dax_file_data: HashMap::new(),
        };

        // Initialize with root inode (nodeid=1, S_IFDIR)
        let root = Inode {
            nodeid: 1,
            name: String::from("/"),
            mode: 0o40755,  // S_IFDIR | 0o755
            size: 4096,
            nlink: 2,
        };
        fs.inodes.insert(1, root);

        // Initialize with "hello.txt" (nodeid=2, S_IFREG, size=4096)
        let file = Inode {
            nodeid: 2,
            name: String::from("hello.txt"),
            mode: 0o100644,  // S_IFREG | 0o644
            size: 4096,
            nlink: 1,
        };
        fs.inodes.insert(2, file);

        // file_data for nodeid=2: filled with 0xAA (FUSE_READ content)
        fs.file_data.insert(2, vec![0xAA; 4096]);

        fs
    }

    pub fn deserialize(_buf: &[u8]) -> Self {
        // Placeholder for deserialization in Task 4
        Self::new()
    }
}
