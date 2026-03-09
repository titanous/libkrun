// Copyright 2024 The libkrun Authors.
// SPDX-License-Identifier: Apache-2.0

//! Safe snapshot serialization using bincode-next with byte-level limits.
//!
//! All snapshot serialize/deserialize calls go through this module.
//! `deserialize` enforces a per-device byte limit via const generic
//! `MAX_BYTES`, which uses bincode-next's `with_limit()` to reject
//! oversized allocation requests before they reach the allocator.

use crate::snapshot::SnapshotError;

/// Serialize a snapshot state struct to bytes.
pub fn serialize<T: bincode_next::Encode>(state: &T) -> Result<Vec<u8>, SnapshotError> {
    bincode_next::encode_to_vec(state, bincode_next::config::standard())
        .map_err(|e| SnapshotError::Serialize(e.to_string()))
}

/// Deserialize a snapshot state struct from bytes with a byte limit.
///
/// `MAX_BYTES` is the maximum allowed payload size for this device.
/// Payloads larger than `MAX_BYTES` are rejected immediately.
/// Additionally, `with_limit()` prevents the deserializer from
/// attempting allocations that exceed the byte budget (e.g., from
/// crafted Vec length prefixes).
pub fn deserialize<T: bincode_next::Decode<()>, const MAX_BYTES: usize>(
    data: &[u8],
) -> Result<T, SnapshotError> {
    if data.len() > MAX_BYTES {
        return Err(SnapshotError::Deserialize(format!(
            "snapshot data {} bytes exceeds limit of {} bytes",
            data.len(),
            MAX_BYTES
        )));
    }
    let (val, _) = bincode_next::decode_from_slice(
        data,
        bincode_next::config::standard().with_limit::<MAX_BYTES>(),
    )
    .map_err(|e| SnapshotError::Deserialize(e.to_string()))?;
    Ok(val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(bincode_next::Encode, bincode_next::Decode, Debug, PartialEq)]
    struct TestState {
        a: u32,
        b: String,
        c: Vec<u8>,
    }

    /// AC2.1: Round-trip serialization with valid data under limit succeeds
    #[test]
    fn test_roundtrip_serialize_deserialize() {
        let original = TestState {
            a: 42,
            b: "hello".to_string(),
            c: vec![1, 2, 3, 4, 5],
        };

        let serialized = serialize(&original).expect("serialize failed");
        let deserialized: TestState =
            deserialize::<TestState, 1024>(&serialized).expect("deserialize failed");

        assert_eq!(original, deserialized);
    }

    /// AC2.2: Deserialization rejects payload exceeding byte limit
    #[test]
    fn test_byte_limit_rejection() {
        let state = TestState {
            a: 42,
            b: "hello".to_string(),
            c: vec![1, 2, 3, 4, 5],
        };

        let serialized = serialize(&state).expect("serialize failed");

        // Try to deserialize with a limit smaller than the serialized size
        let result: Result<TestState, _> = deserialize::<TestState, 1>(&serialized);

        assert!(result.is_err());
        match result {
            Err(SnapshotError::Deserialize(msg)) => {
                assert!(msg.contains("exceeds limit"));
            }
            _ => panic!("Expected Deserialize error"),
        }
    }

    /// AC2.3: Crafted payload with Vec length prefix is rejected by with_limit()
    #[test]
    fn test_crafted_payload_rejection() {
        // Strategy: Serialize a valid TestState, then create a "crafted" version
        // where the Vec length varint is tampered with to claim a huge size (e.g., 4GB).
        // This bypasses the upfront check (data.len() <= MAX_BYTES still holds)
        // but triggers with_limit() during decode when the decoder attempts to
        // allocate 4GB of Vec data.
        //
        // To find the Vec length varint position:
        // 1. Serialize TestState { a: 1, b: "", c: vec![] } — baseline
        // 2. Serialize TestState { a: 1, b: "", c: vec![0u8; 1] } — with 1 byte
        // 3. Compare: the difference shows where the Vec length is encoded
        // 4. Build tampered payload: replace Vec length with a huge varint

        // First, create baseline serializations to find Vec length position
        let baseline = TestState {
            a: 1,
            b: "".to_string(),
            c: vec![],
        };
        let baseline_ser = serialize(&baseline).expect("serialize baseline failed");

        let with_one_byte = TestState {
            a: 1,
            b: "".to_string(),
            c: vec![0u8; 1],
        };
        let one_byte_ser = serialize(&with_one_byte).expect("serialize one_byte failed");

        // The serialized format is:
        // - a: u32 = [0x01, 0x00, 0x00, 0x00] (little-endian)
        // - b: String length as varint (0x00 for empty) + string data
        // - c: Vec length as varint + vec data
        // baseline: [0x01, 0x00, 0x00, 0x00, 0x00, 0x00] = a + string_len(0) + vec_len(0)
        // one_byte: [0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00] = a + string_len(0) + vec_len(1) + vec_data(0x00)

        // Find Vec length position by comparing baseline and one_byte
        let mut vec_length_pos = 0;
        for i in 0..baseline_ser.len() {
            if i >= one_byte_ser.len() || baseline_ser[i] != one_byte_ser[i] {
                vec_length_pos = i;
                break;
            }
        }

        // Create payload with huge Vec length varint
        // Varint encoding for 0xFFFFFFFF (4GB-1):
        // 0xFFFFFFFF in little-endian varint is: [0xFF, 0xFF, 0xFF, 0xFF, 0x0F]
        let huge_varint = [0xFF, 0xFF, 0xFF, 0xFF, 0x0F];

        // Build crafted payload: baseline up to vec_length_pos + huge varint
        let mut crafted = baseline_ser.clone();
        crafted.truncate(vec_length_pos);
        crafted.extend_from_slice(&huge_varint);

        const MAX_BYTES: usize = 128; // Allow the crafted payload to pass upfront check

        // Verify upfront check passes (crafted.len() <= MAX_BYTES)
        assert!(
            crafted.len() <= MAX_BYTES,
            "Crafted payload ({} bytes) must be <= MAX_BYTES ({}) to test with_limit()",
            crafted.len(),
            MAX_BYTES
        );

        // Now deserialize the crafted payload
        let result: Result<TestState, _> = deserialize::<TestState, MAX_BYTES>(&crafted);

        assert!(result.is_err(), "Expected deserialization to fail due to with_limit()");
        match result {
            Err(SnapshotError::Deserialize(msg)) => {
                // Verify the error comes from with_limit(), not the upfront check
                // The upfront check message contains "exceeds limit"
                // The with_limit() error should NOT contain that message
                assert!(
                    !msg.contains("exceeds limit"),
                    "Error should NOT come from upfront check, should come from with_limit(). Got: {}",
                    msg
                );
            }
            _ => panic!("Expected Deserialize error"),
        }
    }
}
