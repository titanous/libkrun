// Copyright 2025 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::cmp::min;

use crate::bus::BusDevice;
use crate::snapshot::{SnapshotError, Snapshottable};

const INDEX_MASK: u8 = 0x7f;
const INDEX_OFFSET: u64 = 0x0;
const DATA_OFFSET: u64 = 0x1;
const DATA_LEN: usize = 128;

#[cfg(feature = "snapshot")]
const MAX_SNAPSHOT_BYTES: usize = 512;

// Fields are only read by serde's generated code (behind the snapshot feature).
// Without --features snapshot, serde derives are absent and fields appear unread.
#[allow(dead_code)]
#[cfg_attr(
    feature = "snapshot",
    derive(bincode_next::Encode, bincode_next::Decode)
)]
#[derive(Debug, Clone)]
struct CmosState {
    index: u8,
    data: Vec<u8>,
}

pub struct Cmos {
    index: u8,
    data: [u8; DATA_LEN],
}

impl Cmos {
    pub fn new(mem_below_4g: u64, mem_above_4g: u64) -> Cmos {
        debug!("cmos: mem_below_4g={mem_below_4g} mem_above_4g={mem_above_4g}");

        let mut data = [0u8; DATA_LEN];

        // Extended memory from 16 MB to 4 GB in units of 64 KB
        let ext_mem = min(
            0xFFFF,
            mem_below_4g.saturating_sub(16 * 1024 * 1024) / (64 * 1024),
        );
        data[0x34] = ext_mem as u8;
        data[0x35] = (ext_mem >> 8) as u8;

        // High memory (> 4GB) in units of 64 KB
        let high_mem = min(0xFFFFFF, mem_above_4g / (64 * 1024));
        data[0x5b] = high_mem as u8;
        data[0x5c] = (high_mem >> 8) as u8;
        data[0x5d] = (high_mem >> 16) as u8;

        Cmos { index: 0, data }
    }
}

impl BusDevice for Cmos {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            error!("cmos: unsupported read length");
            return;
        }

        data[0] = match offset {
            INDEX_OFFSET => {
                debug!("cmos: read index offset");
                self.index
            }
            DATA_OFFSET => {
                debug!("cmos: read data offset from index={:x}", self.index);
                self.data[(self.index & INDEX_MASK) as usize]
            }
            _ => {
                debug!("cmos: unsupported read offset");
                0
            }
        };
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            error!("cmos: unsupported write length");
            return;
        }

        match offset {
            INDEX_OFFSET => {
                debug!("cmos: update index");
                self.index = data[0] & INDEX_MASK;
            }
            _ => debug!("cmos: ignoring unsupported write to CMOS"),
        }
    }

    fn as_snapshottable(&self) -> Option<&dyn Snapshottable> {
        Some(self)
    }

    fn as_snapshottable_mut(&mut self) -> Option<&mut dyn Snapshottable> {
        Some(self)
    }
}

impl Snapshottable for Cmos {
    fn snapshot_id(&self) -> &str {
        "cmos"
    }

    fn save_state(&self) -> std::result::Result<Vec<u8>, SnapshotError> {
        let state = CmosState {
            index: self.index,
            data: self.data[..].to_vec(),
        };

        #[cfg(feature = "snapshot")]
        {
            crate::snapshot_serde::serialize(&state)
        }
        #[cfg(not(feature = "snapshot"))]
        {
            let _ = state;
            Err(SnapshotError::Serialize(
                "snapshot feature not enabled".to_string(),
            ))
        }
    }

    fn restore_state(&mut self, data: &[u8]) -> std::result::Result<(), SnapshotError> {
        #[cfg(feature = "snapshot")]
        {
            let state: CmosState =
                crate::snapshot_serde::deserialize::<_, { MAX_SNAPSHOT_BYTES }>(data)?;
            self.index = state.index;
            if state.data.len() != DATA_LEN {
                return Err(SnapshotError::Deserialize(format!(
                    "CMOS data length mismatch: expected {}, got {}",
                    DATA_LEN,
                    state.data.len()
                )));
            }
            self.data.copy_from_slice(&state.data);
            Ok(())
        }
        #[cfg(not(feature = "snapshot"))]
        {
            let _ = data;
            Err(SnapshotError::Deserialize(
                "snapshot feature not enabled".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "snapshot")]
    fn test_cmos_snapshot_preserves_index_and_data() {
        use super::*;
        // Create a CMOS device with known memory layout
        let mut cmos = Cmos::new(1024 * 1024 * 1024, 0);

        // Write known values to index register
        let data_to_write = [0x42u8];
        cmos.write(0, INDEX_OFFSET, &data_to_write);

        // Verify index was written
        let mut data_read = [0u8; 1];
        cmos.read(0, INDEX_OFFSET, &mut data_read);
        assert_eq!(data_read[0], 0x42);

        // Manually set some data values to verify they're preserved
        cmos.data[0x34] = 0xAA;
        cmos.data[0x35] = 0xBB;
        cmos.data[0x5b] = 0xCC;
        cmos.data[0x5c] = 0xDD;
        cmos.data[0x5d] = 0xEE;

        // Save state
        let saved_state = cmos.save_state().expect("Failed to save CMOS state");

        // Create a fresh CMOS and restore
        let mut cmos_restored = Cmos::new(1024 * 1024 * 1024, 0);
        cmos_restored
            .restore_state(&saved_state)
            .expect("Failed to restore CMOS state");

        // Verify index register
        cmos_restored.read(0, INDEX_OFFSET, &mut data_read);
        assert_eq!(data_read[0], 0x42, "Index register not preserved");

        // Verify all data bytes
        assert_eq!(cmos_restored.data[0x34], 0xAA);
        assert_eq!(cmos_restored.data[0x35], 0xBB);
        assert_eq!(cmos_restored.data[0x5b], 0xCC);
        assert_eq!(cmos_restored.data[0x5c], 0xDD);
        assert_eq!(cmos_restored.data[0x5d], 0xEE);
    }
}
