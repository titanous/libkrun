// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

/// Terminal outcome shared by the KVM and Nitro Sandbox backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmExit {
    /// A device explicitly supplied a workload exit code, not a power-off request.
    Shutdown { exit_code: i32 },
    /// The guest stated that it requested power-off.
    PowerOffRequested,
    /// The guest stated that it requested reboot.
    RebootRequested,
    /// The guest stated that it requested halt.
    Halted,
    /// The guest stated that it panicked.
    Panic,
    /// A terminal guest event occurred without a recognized stated reason.
    Fault,
    /// Host requested an orderly stop through the VM handle.
    Stopped,
    /// Guest agent stopped sending configured liveness heartbeats.
    LivenessExpired,
    /// Guest failed to initialize or recover its device transports in time.
    DeviceWatchdog,
    /// VMM, KVM, or backend failure, not a guest statement.
    Error { message: String },
}

impl VmExit {
    /// Decode the shared stop byte. Invalid bytes do not establish intent.
    pub fn from_stated_reason(reason: u8) -> Self {
        match reason {
            1 => Self::PowerOffRequested,
            2 => Self::RebootRequested,
            3 => Self::Halted,
            4 => Self::Panic,
            _ => Self::Fault,
        }
    }

    /// Host failures and host stop outrank device exit codes and guest intent.
    pub fn resolve(terminal: Self, exit_code: Option<i32>, reason: u8) -> Self {
        match terminal {
            Self::Error { .. } | Self::Stopped => terminal,
            _ if exit_code.is_some() => Self::Shutdown {
                exit_code: exit_code.unwrap(),
            },
            _ if (1..=4).contains(&reason) => Self::from_stated_reason(reason),
            _ => terminal,
        }
    }
}

/// Shared exit state populated before the public run method returns.
pub type SharedVmExit = Arc<Mutex<Option<VmExit>>>;

#[cfg(test)]
mod tests {
    use super::VmExit;

    #[test]
    fn classification_precedence_table() {
        for terminal in [
            VmExit::Fault,
            VmExit::LivenessExpired,
            VmExit::DeviceWatchdog,
            VmExit::Stopped,
            VmExit::Error {
                message: "host failure".into(),
            },
        ] {
            for code in [None, Some(0), Some(42), Some(-1)] {
                for reason in [0, 1, 2, 3, 4, 5, 255] {
                    let expected = if matches!(terminal, VmExit::Error { .. } | VmExit::Stopped) {
                        terminal.clone()
                    } else if let Some(exit_code) = code {
                        VmExit::Shutdown { exit_code }
                    } else if (1..=4).contains(&reason) {
                        VmExit::from_stated_reason(reason)
                    } else {
                        terminal.clone()
                    };
                    assert_eq!(VmExit::resolve(terminal.clone(), code, reason), expected);
                }
            }
        }
    }

    #[test]
    fn stated_reason_table() {
        for (reason, expected) in [
            (0, VmExit::Fault),
            (1, VmExit::PowerOffRequested),
            (2, VmExit::RebootRequested),
            (3, VmExit::Halted),
            (4, VmExit::Panic),
            (5, VmExit::Fault),
            (255, VmExit::Fault),
        ] {
            assert_eq!(VmExit::from_stated_reason(reason), expected);
        }
    }
}
