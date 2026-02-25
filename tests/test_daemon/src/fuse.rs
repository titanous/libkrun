/// FUSE protocol constants and types
/// This module implements the FUSE (Filesystem in Userspace) protocol
/// for the test daemon.

// FUSE opcodes
pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_FORGET: u32 = 2;
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_WRITE: u32 = 16;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_BATCH_FORGET: u32 = 42;
pub const FUSE_SETUPMAPPING: u32 = 48;
pub const FUSE_REMOVEMAPPING: u32 = 49;

// FUSE flags
pub const FUSE_ATTR_DAX: u32 = 2;  // bit 1 in fuse_attr.flags
pub const FUSE_HAS_INODE_DAX: u64 = 0x200000000;  // bit 33 in init flags

// FUSE_INIT defaults
pub const FUSE_MAJOR: u32 = 7;
pub const FUSE_MINOR: u32 = 36;

// FUSE request/response header structures (repr(C) for C compatibility)

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseInHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseOutHeader {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseInitIn {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseInitOut {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u64,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
    pub max_pages: u16,
    pub padding: u16,
    pub reserved: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseEntryOut {
    pub nodeid: u64,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: FuseAttr,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseAttrOut {
    pub attr_valid: u64,
    pub attr_valid_nsec: u32,
    pub dummy: u32,
    pub attr: FuseAttr,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseOpenOut {
    pub fh: u64,
    pub open_flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseReadIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub read_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseSetupmappingIn {
    pub fh: u64,
    pub foffset: u64,
    pub len: u64,
    pub flags: u32,
    pub padding: u32,
    pub moffset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FuseRemovemappingIn {
    pub fh: u64,
    pub foffset: u64,
    pub len: u64,
}

// Helper function to serialize a structure to bytes
pub fn struct_to_bytes<T: Sized>(s: &T) -> Vec<u8> {
    unsafe {
        let ptr = s as *const T as *const u8;
        let len = std::mem::size_of::<T>();
        std::slice::from_raw_parts(ptr, len).to_vec()
    }
}

// Helper function to read a structure from a slice
pub fn bytes_to_struct<T: Sized>(bytes: &[u8]) -> Option<T> {
    if bytes.len() < std::mem::size_of::<T>() {
        return None;
    }
    unsafe {
        let ptr = bytes.as_ptr() as *const T;
        Some(std::ptr::read(ptr))
    }
}
