// Copyright 2019 The Chromium OS Authors. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

//! Pure FUSE protocol routing: header length validation and opcode classification.
//! No FileSystem interaction; no I/O; testable under Miri.

use super::fuse::Opcode;

pub const MAX_BUFFER_SIZE: u32 = 1 << 20;
pub(super) const BUFFER_HEADER_SIZE: u32 = 0x1000;

/// Validate that a FUSE message length is within the allowed limit.
///
/// Returns `true` if the message is within bounds and can be processed.
pub(super) fn is_valid_len(len: u32) -> bool {
    len <= MAX_BUFFER_SIZE + BUFFER_HEADER_SIZE
}

/// Map a raw FUSE opcode integer to the typed `Opcode` enum.
///
/// Returns `None` for unknown opcodes; callers should reply with ENOSYS.
pub(super) fn classify_opcode(opcode: u32) -> Option<Opcode> {
    // Each arm maps the typed enum variant to its u32 wire value.
    match opcode {
        x if x == Opcode::Lookup as u32 => Some(Opcode::Lookup),
        x if x == Opcode::Forget as u32 => Some(Opcode::Forget),
        x if x == Opcode::Getattr as u32 => Some(Opcode::Getattr),
        x if x == Opcode::Setattr as u32 => Some(Opcode::Setattr),
        x if x == Opcode::Readlink as u32 => Some(Opcode::Readlink),
        x if x == Opcode::Symlink as u32 => Some(Opcode::Symlink),
        x if x == Opcode::Mknod as u32 => Some(Opcode::Mknod),
        x if x == Opcode::Mkdir as u32 => Some(Opcode::Mkdir),
        x if x == Opcode::Unlink as u32 => Some(Opcode::Unlink),
        x if x == Opcode::Rmdir as u32 => Some(Opcode::Rmdir),
        x if x == Opcode::Rename as u32 => Some(Opcode::Rename),
        x if x == Opcode::Link as u32 => Some(Opcode::Link),
        x if x == Opcode::Open as u32 => Some(Opcode::Open),
        x if x == Opcode::Read as u32 => Some(Opcode::Read),
        x if x == Opcode::Write as u32 => Some(Opcode::Write),
        x if x == Opcode::Statfs as u32 => Some(Opcode::Statfs),
        x if x == Opcode::Release as u32 => Some(Opcode::Release),
        x if x == Opcode::Fsync as u32 => Some(Opcode::Fsync),
        x if x == Opcode::Setxattr as u32 => Some(Opcode::Setxattr),
        x if x == Opcode::Getxattr as u32 => Some(Opcode::Getxattr),
        x if x == Opcode::Listxattr as u32 => Some(Opcode::Listxattr),
        x if x == Opcode::Removexattr as u32 => Some(Opcode::Removexattr),
        x if x == Opcode::Flush as u32 => Some(Opcode::Flush),
        x if x == Opcode::Init as u32 => Some(Opcode::Init),
        x if x == Opcode::Opendir as u32 => Some(Opcode::Opendir),
        x if x == Opcode::Readdir as u32 => Some(Opcode::Readdir),
        x if x == Opcode::Releasedir as u32 => Some(Opcode::Releasedir),
        x if x == Opcode::Fsyncdir as u32 => Some(Opcode::Fsyncdir),
        x if x == Opcode::Getlk as u32 => Some(Opcode::Getlk),
        x if x == Opcode::Setlk as u32 => Some(Opcode::Setlk),
        x if x == Opcode::Setlkw as u32 => Some(Opcode::Setlkw),
        x if x == Opcode::Access as u32 => Some(Opcode::Access),
        x if x == Opcode::Create as u32 => Some(Opcode::Create),
        x if x == Opcode::Interrupt as u32 => Some(Opcode::Interrupt),
        x if x == Opcode::Bmap as u32 => Some(Opcode::Bmap),
        x if x == Opcode::Destroy as u32 => Some(Opcode::Destroy),
        x if x == Opcode::Ioctl as u32 => Some(Opcode::Ioctl),
        x if x == Opcode::Poll as u32 => Some(Opcode::Poll),
        x if x == Opcode::NotifyReply as u32 => Some(Opcode::NotifyReply),
        x if x == Opcode::BatchForget as u32 => Some(Opcode::BatchForget),
        x if x == Opcode::Fallocate as u32 => Some(Opcode::Fallocate),
        x if x == Opcode::Readdirplus as u32 => Some(Opcode::Readdirplus),
        x if x == Opcode::Rename2 as u32 => Some(Opcode::Rename2),
        x if x == Opcode::Lseek as u32 => Some(Opcode::Lseek),
        x if x == Opcode::CopyFileRange as u32 => Some(Opcode::CopyFileRange),
        x if x == Opcode::SetupMapping as u32 => Some(Opcode::SetupMapping),
        x if x == Opcode::RemoveMapping as u32 => Some(Opcode::RemoveMapping),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_len_boundary() {
        assert!(is_valid_len(0));
        assert!(is_valid_len(MAX_BUFFER_SIZE + BUFFER_HEADER_SIZE));
        assert!(!is_valid_len(MAX_BUFFER_SIZE + BUFFER_HEADER_SIZE + 1));
        assert!(!is_valid_len(u32::MAX));
    }

    #[test]
    fn test_classify_opcode_known() {
        // Opcode::Lookup == 1 per FUSE protocol
        assert!(classify_opcode(Opcode::Lookup as u32).is_some());
        assert!(classify_opcode(Opcode::Init as u32).is_some());
        assert!(classify_opcode(Opcode::Destroy as u32).is_some());
    }

    #[test]
    fn test_classify_opcode_unknown() {
        assert!(classify_opcode(0).is_none());
        assert!(classify_opcode(9999).is_none());
    }
}
