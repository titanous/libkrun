use std::collections::HashMap;

#[derive(Clone)]
pub struct Inode {
    pub nodeid: u64,
    pub name: String,
    pub mode: u32, // S_IFREG | 0o644
    pub size: u64,
    pub nlink: u32,
    pub dax_enabled: bool,
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
            mode: 0o40755, // S_IFDIR | 0o755
            size: 4096,
            nlink: 2,
            dax_enabled: false,
        };
        fs.inodes.insert(1, root);

        // Initialize with "hello.txt" (nodeid=2, S_IFREG, size=4096, DAX enabled)
        let file = Inode {
            nodeid: 2,
            name: String::from("hello.txt"),
            mode: 0o100644, // S_IFREG | 0o644
            size: 4096,
            nlink: 1,
            dax_enabled: true,
        };
        fs.inodes.insert(2, file);

        // file_data for nodeid=2: filled with 0xAA (FUSE_READ content)
        fs.file_data.insert(2, vec![0xAA; 4096]);

        // Initialize with "nodax.txt" (nodeid=3, S_IFREG, size=4096, DAX disabled)
        let nodax_file = Inode {
            nodeid: 3,
            name: String::from("nodax.txt"),
            mode: 0o100644, // S_IFREG | 0o644
            size: 4096,
            nlink: 1,
            dax_enabled: false,
        };
        fs.inodes.insert(3, nodax_file);

        // file_data for nodeid=3: filled with 0xAA (FUSE_READ content)
        fs.file_data.insert(3, vec![0xAA; 4096]);

        fs
    }

    pub fn deserialize(buf: &[u8]) -> std::io::Result<Self> {
        let mut fs = SyntheticFs {
            inodes: HashMap::new(),
            file_data: HashMap::new(),
            dax_pattern: 0xBB,
            dax_file_data: HashMap::new(),
        };

        if buf.len() < 4 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "buffer too short",
            ));
        }

        let num_inodes = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let mut offset = 4;

        for _ in 0..num_inodes {
            // Read nodeid
            if offset + 8 > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated nodeid",
                ));
            }
            let nodeid = u64::from_le_bytes([
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
                buf[offset + 4],
                buf[offset + 5],
                buf[offset + 6],
                buf[offset + 7],
            ]);
            offset += 8;

            // Read name_len
            if offset + 4 > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated name_len",
                ));
            }
            let name_len = u32::from_le_bytes([
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
            ]) as usize;
            offset += 4;

            // Read name
            if offset + name_len > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated name",
                ));
            }
            let name = String::from_utf8_lossy(&buf[offset..offset + name_len]).to_string();
            offset += name_len;

            // Read mode
            if offset + 4 > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated mode",
                ));
            }
            let mode = u32::from_le_bytes([
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
            ]);
            offset += 4;

            // Read size
            if offset + 8 > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated size",
                ));
            }
            let size = u64::from_le_bytes([
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
                buf[offset + 4],
                buf[offset + 5],
                buf[offset + 6],
                buf[offset + 7],
            ]);
            offset += 8;

            // Read nlink
            if offset + 4 > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated nlink",
                ));
            }
            let nlink = u32::from_le_bytes([
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
            ]);
            offset += 4;

            // Read dax_enabled
            if offset + 1 > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated dax_enabled",
                ));
            }
            let dax_enabled = buf[offset] != 0;
            offset += 1;

            // Read data_len
            if offset + 4 > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated data_len",
                ));
            }
            let data_len = u32::from_le_bytes([
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
            ]) as usize;
            offset += 4;

            // Read data
            if offset + data_len > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "truncated data",
                ));
            }
            let data = buf[offset..offset + data_len].to_vec();
            offset += data_len;

            // Create inode
            let inode = Inode {
                nodeid,
                name,
                mode,
                size,
                nlink,
                dax_enabled,
            };
            fs.inodes.insert(nodeid, inode);

            // Store file data if present
            if data_len > 0 {
                fs.file_data.insert(nodeid, data);
            }
        }

        Ok(fs)
    }
}
