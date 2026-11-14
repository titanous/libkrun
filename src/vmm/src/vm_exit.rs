// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use std::sync::{Arc, Mutex};

/// Terminal outcome shared by the KVM and Nitro Sandbox backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmExit {
    /// An explicit workload exit code, including zero; not a kernel power-off.
    Shutdown { exit_code: i32 },
    /// The guest kernel stated power-off intent before its terminal event.
    PowerOffRequested,
    /// The guest kernel requested reboot; the VMM does not restart it in place.
    RebootRequested,
    /// The guest kernel requested halt.
    Halted,
    /// The guest kernel reported panic; this report is sticky.
    Panic,
    /// A guest terminal event without a valid final reason, or invalid guest state.
    Fault,
    /// The host explicitly stopped the guest.
    Stopped,
    /// The host's guest-liveness deadline expired.
    LivenessExpired,
    /// The host's device-progress deadline expired.
    DeviceWatchdog,
    /// A host/backend failure, distinct from guest failure.
    Error { message: String },
}
impl VmExit {
    pub fn from_stated_reason(reason: u8) -> Self {
        match reason {
            1 => Self::PowerOffRequested,
            2 => Self::RebootRequested,
            3 => Self::Halted,
            4 => Self::Panic,
            _ => Self::Fault,
        }
    }
    /// One classification for both backends. Call only after a terminal event.
    /// Error > host stop > explicit code > expired deadline > stated reason > fault.
    pub fn resolve(terminal: Self, exit_code: Option<i32>, reason: u8) -> Self {
        match terminal {
            Self::Error { .. } | Self::Stopped => terminal,
            _ if exit_code.is_some() => Self::Shutdown {
                exit_code: exit_code.unwrap(),
            },
            Self::LivenessExpired | Self::DeviceWatchdog => terminal,
            _ => Self::from_stated_reason(reason),
        }
    }
}
pub type SharedVmExit = Arc<Mutex<Option<VmExit>>>;

#[cfg(test)]
mod tests {
    use super::VmExit;
    use super::VmExit::*;
    #[test]
    fn classification_precedence_table() {
        for (terminal, code, reason, expected) in [
            (Fault, None, 0, Fault),
            (Fault, None, 255, Fault),
            (Fault, None, 1, PowerOffRequested),
            (Fault, None, 2, RebootRequested),
            (Fault, None, 3, Halted),
            (Fault, None, 4, Panic),
            (Fault, Some(0), 4, Shutdown { exit_code: 0 }),
            (Fault, Some(37), 1, Shutdown { exit_code: 37 }),
            (Stopped, Some(1), 4, Stopped),
            (Stopped, None, 2, Stopped),
            (LivenessExpired, None, 4, LivenessExpired),
            (DeviceWatchdog, None, 1, DeviceWatchdog),
            (LivenessExpired, Some(9), 4, Shutdown { exit_code: 9 }),
            (
                Error {
                    message: "host".into(),
                },
                Some(0),
                1,
                Error {
                    message: "host".into(),
                },
            ),
        ] {
            assert_eq!(VmExit::resolve(terminal, code, reason), expected);
        }
    }
}
