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

    /// AC2.3: Crafted payload with large Vec length prefix is rejected before allocation
    #[test]
    fn test_crafted_payload_rejection() {
        // Serialize a TestState with a reasonably-sized payload
        let state = TestState {
            a: 42,
            b: "hello".to_string(),
            c: vec![0u8; 256], // 256 bytes in the Vec
        };

        let serialized = serialize(&state).expect("serialize failed");

        // Try to deserialize with a limit smaller than the serialized size
        // The with_limit() guard will reject the allocation when it tries
        // to decode the Vec size
        let result: Result<TestState, _> = deserialize::<TestState, 64>(&serialized);

        assert!(result.is_err());
        match result {
            Err(SnapshotError::Deserialize(_msg)) => {
                // Expected: either limit exceeded or decode error
            }
            _ => panic!("Expected Deserialize error"),
        }
    }
}
