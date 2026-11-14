//! Private RAM copying for a parked Nitro receiver.
//!
//! Offsets refer to validated guest-owned mappings, never the driver's complete
//! allocation (which can include firmware-owned pages that block host reads).
use std::{io, ops::Range};

/// Copy an owned RAM extent while preserving the receiver's reserved bytes.
///
/// The caller obtains exclusive slices only after the source is frozen, all
/// device workers have stopped, and the receiver has published READY. Validate
/// every extent before calling this function for the first extent of a template.
pub fn copy_extent(
    source: &[u8],
    destination: &mut [u8],
    reserved: Range<usize>,
    threads: usize,
) -> io::Result<()> {
    if source.len() != destination.len()
        || reserved.start > reserved.end
        || reserved.end > source.len()
        || threads == 0
        || threads > 16
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Nitro memory copy extent",
        ));
    }
    let (before, tail) = destination.split_at_mut(reserved.start);
    let after = &mut tail[reserved.end - reserved.start..];
    // Every byte outside the reservation is overwritten, including zero pages:
    // a newly booted receiver contains firmware data, not only zeroed RAM.
    for (input, output) in [
        (&source[..reserved.start], before),
        (&source[reserved.end..], after),
    ] {
        std::thread::scope(|scope| {
            let chunk = input.len().div_ceil(threads).max(1);
            for (input, output) in input.chunks(chunk).zip(output.chunks_mut(chunk)) {
                scope.spawn(move || output.copy_from_slice(input));
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_stub_and_overwrites_zeroes_and_tail() {
        for threads in [1, 4, 8, 16] {
            let mut source = vec![0; 100_003];
            source[0] = 3;
            source[100_002] = 7;
            let mut destination = vec![0xa5; source.len()];
            copy_extent(&source, &mut destination, 4096..8192, threads).unwrap();
            assert_eq!(&source[..4096], &destination[..4096]);
            assert_eq!(&source[8192..], &destination[8192..]);
            assert!(destination[4096..8192].iter().all(|byte| *byte == 0xa5));
        }
    }

    #[test]
    fn invalid_extent_does_not_modify_receiver() {
        for (size, reserved, threads) in [
            (16, 0..17, 1),
            (16, 9..8, 1),
            (16, 0..0, 0),
            (16, 0..0, 17),
            (15, 0..0, 1),
        ] {
            let mut destination = [0xa5; 16];
            assert!(copy_extent(&vec![0; size], &mut destination, reserved, threads).is_err());
            assert_eq!(destination, [0xa5; 16]);
        }
    }

    #[test]
    fn permits_empty_reservation_and_empty_extent() {
        let mut destination = [0xa5; 7];
        copy_extent(&[1; 7], &mut destination, 0..0, 16).unwrap();
        assert_eq!(destination, [1; 7]);
        copy_extent(&[], &mut [], 0..0, 16).unwrap();
    }
}
