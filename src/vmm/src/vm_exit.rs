// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

/// Reason the VM exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmExit {
    /// Guest initiated a clean shutdown (HLT or ACPI shutdown).
    /// Contains the exit code set by the guest (0 = success).
    Shutdown { exit_code: i32 },

    /// Guest cooperatively froze; its immutable template was written to this path.
    Frozen { template_path: std::path::PathBuf },

    /// Guest requested a reboot (KVM_SYSTEM_EVENT_RESET).
    RebootRequested,

    /// Host requested an orderly stop through the VM handle.
    Stopped,

    /// Maintained guest kernel emitted its panic stop marker.
    GuestPanic,
    /// Guest agent stopped sending configured liveness heartbeats.
    LivenessExpired,
    /// Guest failed to initialize or recover its device transports in time.
    DeviceWatchdog,

    /// vCPU encountered a fatal error (triple fault, internal KVM error).
    Error { message: String },
}

/// Shared exit state: the VMM event loop stores `Some(VmExit)` here when the
/// VM exits, and `Context::run()` reads it to break the event loop.
pub type SharedVmExit = Arc<Mutex<Option<VmExit>>>;
