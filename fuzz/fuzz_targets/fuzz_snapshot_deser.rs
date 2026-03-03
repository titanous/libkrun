#![no_main]

use libfuzzer_sys::fuzz_target;
use vmm::snapshot::{IncrementalSnapshot, VmSnapshot};

fuzz_target!(|data: &[u8]| {
    // Attempt to deserialize as VmSnapshot.
    // bincode::deserialize must not panic on arbitrary input.
    // Errors (e.g., unexpected end of input, invalid enum variant) are expected and fine.
    let _ = bincode::deserialize::<VmSnapshot>(data);

    // Also attempt to deserialize as IncrementalSnapshot.
    let _ = bincode::deserialize::<IncrementalSnapshot>(data);
});
