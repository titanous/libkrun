// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

macro_rules! generate_read_fn {
    ($fn_name: ident, $data_type: ty, $byte_type: ty, $type_size: expr, $endian_type: ident) => {
        #[cfg_attr(kani, kani::requires(input.len() >= $type_size))]
        pub fn $fn_name(input: &[$byte_type]) -> $data_type {
            assert!($type_size == std::mem::size_of::<$data_type>());
            let mut array = [0u8; $type_size];
            for (byte, read) in array.iter_mut().zip(input.iter().cloned()) {
                *byte = read as u8;
            }
            <$data_type>::$endian_type(array)
        }
    };
}

macro_rules! generate_write_fn {
    ($fn_name: ident, $data_type: ty, $byte_type: ty, $endian_type: ident) => {
        #[cfg_attr(kani, kani::requires(buf.len() >= std::mem::size_of::<$data_type>()))]
        #[cfg_attr(kani, kani::modifies(buf))]
        pub fn $fn_name(buf: &mut [$byte_type], n: $data_type) {
            for (byte, read) in buf
                .iter_mut()
                .zip(<$data_type>::$endian_type(n).iter().cloned())
            {
                *byte = read as $byte_type;
            }
        }
    };
}

generate_read_fn!(read_le_u16, u16, u8, 2, from_le_bytes);
generate_read_fn!(read_le_u32, u32, u8, 4, from_le_bytes);
generate_read_fn!(read_le_u64, u64, u8, 8, from_le_bytes);
generate_read_fn!(read_le_i32, i32, i8, 4, from_le_bytes);

generate_read_fn!(read_be_u16, u16, u8, 2, from_be_bytes);
generate_read_fn!(read_be_u32, u32, u8, 4, from_be_bytes);

generate_write_fn!(write_le_u16, u16, u8, to_le_bytes);
generate_write_fn!(write_le_u32, u32, u8, to_le_bytes);
generate_write_fn!(write_le_u64, u64, u8, to_le_bytes);
generate_write_fn!(write_le_i32, i32, i8, to_le_bytes);

generate_write_fn!(write_be_u16, u16, u8, to_be_bytes);
generate_write_fn!(write_be_u32, u32, u8, to_be_bytes);

#[cfg(test)]
mod tests {
    use super::*;
    macro_rules! byte_order_test_read_write {
        ($test_name: ident, $write_fn_name: ident, $read_fn_name: ident, $is_be: expr, $data_type: ty) => {
            #[test]
            fn $test_name() {
                #[allow(overflowing_literals)]
                let test_cases = [
                    (
                        0x0123_4567_89AB_CDEF as u64,
                        [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef],
                    ),
                    (
                        0x0000_0000_0000_0000 as u64,
                        [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
                    ),
                    (
                        0x1923_2345_ABF3_CCD4 as u64,
                        [0x19, 0x23, 0x23, 0x45, 0xAB, 0xF3, 0xCC, 0xD4],
                    ),
                    (
                        0x0FF0_0FF0_0FF0_0FF0 as u64,
                        [0x0F, 0xF0, 0x0F, 0xF0, 0x0F, 0xF0, 0x0F, 0xF0],
                    ),
                    (
                        0xFFFF_FFFF_FFFF_FFFF as u64,
                        [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
                    ),
                    (
                        0x89AB_12D4_C2D2_09BB as u64,
                        [0x89, 0xAB, 0x12, 0xD4, 0xC2, 0xD2, 0x09, 0xBB],
                    ),
                ];

                let type_size = std::mem::size_of::<$data_type>();
                for (test_val, v_arr) in &test_cases {
                    let v = *test_val as $data_type;
                    let cmp_iter: Box<dyn Iterator<Item = _>> = if $is_be {
                        Box::new(v_arr[(8 - type_size)..].iter())
                    } else {
                        Box::new(v_arr.iter().rev())
                    };
                    // test write
                    let mut write_arr = vec![Default::default(); type_size];
                    $write_fn_name(&mut write_arr, v);
                    for (cmp, cur) in cmp_iter.zip(write_arr.iter()) {
                        assert_eq!(*cmp, *cur as u8)
                    }
                    // test read
                    let read_val = $read_fn_name(&write_arr);
                    assert_eq!(v, read_val);
                }
            }
        };
    }

    byte_order_test_read_write!(test_le_u16, write_le_u16, read_le_u16, false, u16);
    byte_order_test_read_write!(test_le_u32, write_le_u32, read_le_u32, false, u32);
    byte_order_test_read_write!(test_le_u64, write_le_u64, read_le_u64, false, u64);
    byte_order_test_read_write!(test_le_i32, write_le_i32, read_le_i32, false, i32);
    byte_order_test_read_write!(test_be_u16, write_be_u16, read_be_u16, true, u16);
    byte_order_test_read_write!(test_be_u32, write_be_u32, read_be_u32, true, u32);
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// write_le_u16 followed by read_le_u16 is identity for all u16 values.
    ///
    /// Verifies the little-endian u16 round-trip: write then read returns the
    /// original value. Would fail if to_le_bytes or from_le_bytes were swapped
    /// with big-endian variants, or if the byte loop wrote incorrect indices.
    ///
    /// Bound: 2-byte loop unwinds at 3 (2 + 1).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(3)]
    fn proof_le_u16_roundtrip() {
        let val: u16 = kani::any();
        let mut buf = [0u8; 2];
        write_le_u16(&mut buf, val);
        let result = read_le_u16(&buf);
        kani::assert(result == val, "le_u16 write-read must be identity");
        kani::cover!(val == 0, "zero value exercised");
        kani::cover!(val == u16::MAX, "max value exercised");
        kani::cover!(val & 0xFF00 != 0, "high byte non-zero exercised");
    }

    /// write_le_u32 followed by read_le_u32 is identity for all u32 values.
    ///
    /// Verifies the little-endian u32 round-trip. Would fail if endian conversion
    /// used big-endian bytes or if the write/read loops had incorrect stride.
    ///
    /// Bound: 4-byte loop unwinds at 5 (4 + 1).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_le_u32_roundtrip() {
        let val: u32 = kani::any();
        let mut buf = [0u8; 4];
        write_le_u32(&mut buf, val);
        let result = read_le_u32(&buf);
        kani::assert(result == val, "le_u32 write-read must be identity");
        kani::cover!(val == 0, "zero value exercised");
        kani::cover!(val == u32::MAX, "max value exercised");
        kani::cover!(val & 0xFF00_0000 != 0, "high byte non-zero exercised");
    }

    /// write_le_u64 followed by read_le_u64 is identity for all u64 values.
    ///
    /// Verifies the little-endian u64 round-trip. Would fail if the 8-byte loop
    /// truncated to fewer bytes or used wrong endian conversion.
    ///
    /// Bound: 8-byte loop unwinds at 9 (8 + 1).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(9)]
    fn proof_le_u64_roundtrip() {
        let val: u64 = kani::any();
        let mut buf = [0u8; 8];
        write_le_u64(&mut buf, val);
        let result = read_le_u64(&buf);
        kani::assert(result == val, "le_u64 write-read must be identity");
        kani::cover!(val == 0, "zero value exercised");
        kani::cover!(val == u64::MAX, "max value exercised");
        kani::cover!(
            val & 0xFF00_0000_0000_0000 != 0,
            "high byte non-zero exercised"
        );
    }

    /// write_le_i32 followed by read_le_i32 is identity for all i32 values.
    ///
    /// Verifies the signed little-endian i32 round-trip. Would fail if sign
    /// extension was applied incorrectly or byte_type cast lost bits.
    ///
    /// Bound: 4-byte loop unwinds at 5 (4 + 1).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_le_i32_roundtrip() {
        let val: i32 = kani::any();
        let mut buf = [0i8; 4];
        write_le_i32(&mut buf, val);
        let result = read_le_i32(&buf);
        kani::assert(result == val, "le_i32 write-read must be identity");
        kani::cover!(val == 0, "zero value exercised");
        kani::cover!(val == i32::MIN, "min (most-negative) value exercised");
        kani::cover!(val == i32::MAX, "max value exercised");
        kani::cover!(val < 0, "negative value exercised");
    }

    /// write_be_u16 followed by read_be_u16 is identity for all u16 values.
    ///
    /// Verifies the big-endian u16 round-trip. Would fail if to_be_bytes /
    /// from_be_bytes were replaced with little-endian variants.
    ///
    /// Bound: 2-byte loop unwinds at 3 (2 + 1).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(3)]
    fn proof_be_u16_roundtrip() {
        let val: u16 = kani::any();
        let mut buf = [0u8; 2];
        write_be_u16(&mut buf, val);
        let result = read_be_u16(&buf);
        kani::assert(result == val, "be_u16 write-read must be identity");
        kani::cover!(val == 0, "zero value exercised");
        kani::cover!(val == u16::MAX, "max value exercised");
        kani::cover!(val & 0x00FF != 0, "low byte non-zero exercised");
    }

    /// write_be_u32 followed by read_be_u32 is identity for all u32 values.
    ///
    /// Verifies the big-endian u32 round-trip. Would fail if endian conversion
    /// was little-endian or if byte order was partially correct.
    ///
    /// Bound: 4-byte loop unwinds at 5 (4 + 1).
    #[kani::proof]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_be_u32_roundtrip() {
        let val: u32 = kani::any();
        let mut buf = [0u8; 4];
        write_be_u32(&mut buf, val);
        let result = read_be_u32(&buf);
        kani::assert(result == val, "be_u32 write-read must be identity");
        kani::cover!(val == 0, "zero value exercised");
        kani::cover!(val == u32::MAX, "max value exercised");
        kani::cover!(val & 0x0000_00FF != 0, "low byte non-zero exercised");
    }

    // ── Contract-based proofs ─────────────────────────────────────────────────
    // proof_for_contract harnesses verify the `#[kani::requires]` precondition
    // on each function (buf/input length >= type size).  proof_for_contract
    // instruments the call site so Kani checks the precondition holds before
    // allowing the body to execute.
    //
    // Read-side harnesses use fixed-size arrays (the only way to construct a
    // slice of known sufficient length without Vec).  Write-side harnesses use
    // fixed-size mutable arrays of exactly the required size; proof_for_contract
    // ensures the `requires` annotation on the generated function is checked.

    /// write_le_u16 requires buf.len() >= 2; proof_for_contract checks the precondition.
    ///
    /// Kani instruments the call so the `requires(buf.len() >= 2)` annotation is
    /// verified at the call site. Would fail if the requires bound were lowered
    /// below 2 while the function still indexes buf[1].
    ///
    /// Bound: 2-byte write loop unwinds at 3 (2 + 1).
    #[kani::proof_for_contract(write_le_u16)]
    #[kani::solver(cadical)]
    #[kani::unwind(3)]
    fn proof_contract_write_le_u16() {
        let val: u16 = kani::any();
        let mut buf = [0u8; 2];
        write_le_u16(&mut buf, val);
    }

    /// read_le_u16 requires input.len() >= 2; proof_for_contract checks the precondition.
    ///
    /// Bound: 2-byte read loop unwinds at 3 (2 + 1).
    #[kani::proof_for_contract(read_le_u16)]
    #[kani::solver(cadical)]
    #[kani::unwind(3)]
    fn proof_contract_read_le_u16() {
        let buf = [kani::any::<u8>(), kani::any::<u8>()];
        let _ = read_le_u16(&buf);
    }

    /// write_le_u32 requires buf.len() >= 4; proof_for_contract checks the precondition.
    ///
    /// Would fail if the requires bound were lowered below 4 while the function
    /// still writes buf[3].
    ///
    /// Bound: 4-byte write loop unwinds at 5 (4 + 1).
    #[kani::proof_for_contract(write_le_u32)]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_contract_write_le_u32() {
        let val: u32 = kani::any();
        let mut buf = [0u8; 4];
        write_le_u32(&mut buf, val);
    }

    /// read_le_u32 requires input.len() >= 4; proof_for_contract checks the precondition.
    ///
    /// Bound: 4-byte read loop unwinds at 5 (4 + 1).
    #[kani::proof_for_contract(read_le_u32)]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_contract_read_le_u32() {
        let buf = [kani::any::<u8>(); 4];
        let _ = read_le_u32(&buf);
    }

    /// write_le_u64 requires buf.len() >= 8; proof_for_contract checks the precondition.
    ///
    /// Would fail if the requires bound were lowered below 8 while the function
    /// still writes buf[7].
    ///
    /// Bound: 8-byte write loop unwinds at 9 (8 + 1).
    #[kani::proof_for_contract(write_le_u64)]
    #[kani::solver(cadical)]
    #[kani::unwind(9)]
    fn proof_contract_write_le_u64() {
        let val: u64 = kani::any();
        let mut buf = [0u8; 8];
        write_le_u64(&mut buf, val);
    }

    /// read_le_u64 requires input.len() >= 8; proof_for_contract checks the precondition.
    ///
    /// Bound: 8-byte read loop unwinds at 9 (8 + 1).
    #[kani::proof_for_contract(read_le_u64)]
    #[kani::solver(cadical)]
    #[kani::unwind(9)]
    fn proof_contract_read_le_u64() {
        let buf = [kani::any::<u8>(); 8];
        let _ = read_le_u64(&buf);
    }

    /// write_be_u16 requires buf.len() >= 2; proof_for_contract checks the precondition.
    ///
    /// Bound: 2-byte write loop unwinds at 3 (2 + 1).
    #[kani::proof_for_contract(write_be_u16)]
    #[kani::solver(cadical)]
    #[kani::unwind(3)]
    fn proof_contract_write_be_u16() {
        let val: u16 = kani::any();
        let mut buf = [0u8; 2];
        write_be_u16(&mut buf, val);
    }

    /// read_be_u16 requires input.len() >= 2; proof_for_contract checks the precondition.
    ///
    /// Bound: 2-byte read loop unwinds at 3 (2 + 1).
    #[kani::proof_for_contract(read_be_u16)]
    #[kani::solver(cadical)]
    #[kani::unwind(3)]
    fn proof_contract_read_be_u16() {
        let buf = [kani::any::<u8>(), kani::any::<u8>()];
        let _ = read_be_u16(&buf);
    }

    /// write_be_u32 requires buf.len() >= 4; proof_for_contract checks the precondition.
    ///
    /// Bound: 4-byte write loop unwinds at 5 (4 + 1).
    #[kani::proof_for_contract(write_be_u32)]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_contract_write_be_u32() {
        let val: u32 = kani::any();
        let mut buf = [0u8; 4];
        write_be_u32(&mut buf, val);
    }

    /// read_be_u32 requires input.len() >= 4; proof_for_contract checks the precondition.
    ///
    /// Bound: 4-byte read loop unwinds at 5 (4 + 1).
    #[kani::proof_for_contract(read_be_u32)]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_contract_read_be_u32() {
        let buf = [kani::any::<u8>(); 4];
        let _ = read_be_u32(&buf);
    }

    /// write_le_i32 requires buf.len() >= 4; proof_for_contract checks the precondition.
    ///
    /// Would fail if the requires bound were lowered below 4 while the function
    /// still writes buf[3] (signed i8 slice).
    ///
    /// Bound: 4-byte write loop unwinds at 5 (4 + 1).
    #[kani::proof_for_contract(write_le_i32)]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_contract_write_le_i32() {
        let val: i32 = kani::any();
        let mut buf = [0i8; 4];
        write_le_i32(&mut buf, val);
    }

    /// read_le_i32 requires input.len() >= 4; proof_for_contract checks the precondition.
    ///
    /// Bound: 4-byte read loop unwinds at 5 (4 + 1).
    #[kani::proof_for_contract(read_le_i32)]
    #[kani::solver(cadical)]
    #[kani::unwind(5)]
    fn proof_contract_read_le_i32() {
        let buf = [kani::any::<i8>(); 4];
        let _ = read_le_i32(&buf);
    }
}
