use std::collections::HashMap;

#[derive(Clone)]
pub struct Inode {
    pub nodeid: u64,
    pub name: String,
    pub mode: u32,
    pub size: u64,
    pub nlink: u32,
    pub dax_enabled: bool,
}

pub struct SyntheticFs {
    pub inodes: HashMap<u64, Inode>,
    /// FUSE_READ content differs from the DAX pattern to detect the read path.
    pub file_data: HashMap<u64, Vec<u8>>,
    pub dax_pattern: u8,
}

impl SyntheticFs {
    pub fn new() -> Self {
        let mut fs = Self {
            inodes: HashMap::new(),
            file_data: HashMap::new(),
            dax_pattern: 0xBB,
        };
        for (nodeid, name, mode, dax_enabled) in [
            (1, "/", 0o40755, false),
            (2, "hello.txt", 0o100644, true),
            (3, "nodax.txt", 0o100644, false),
        ] {
            fs.inodes.insert(
                nodeid,
                Inode {
                    nodeid,
                    name: name.to_owned(),
                    mode,
                    size: 4096,
                    nlink: if nodeid == 1 { 2 } else { 1 },
                    dax_enabled,
                },
            );
            if nodeid != 1 {
                fs.file_data.insert(nodeid, vec![0xAA; 4096]);
            }
        }
        fs
    }
}
