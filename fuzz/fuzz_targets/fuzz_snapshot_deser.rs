#![no_main]

use libfuzzer_sys::fuzz_target;
use vmm::snapshot::{IncrementalSnapshot, VmSnapshot};

fuzz_target!(|data: &[u8]| {
    // Attempt to deserialize as VmSnapshot.
    // bincode_next::decode_from_slice must not panic on arbitrary input.
    // Errors (e.g., unexpected end of input, invalid enum variant) are expected and fine.
    let _ = bincode_next::decode_from_slice::<VmSnapshot, _>(data, bincode_next::config::standard());

    // Also attempt to deserialize as IncrementalSnapshot.
    let _ = bincode_next::decode_from_slice::<IncrementalSnapshot, _>(data, bincode_next::config::standard());
});
