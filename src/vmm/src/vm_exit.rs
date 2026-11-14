// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex};

/// Reason the VM exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmExit {
    /// Guest explicitly requested power-off through the stop-reason protocol.
    PowerOffRequested,
    /// Guest explicitly halted all CPUs with interrupts disabled.
    Halted,
    /// Guest kernel reported a panic before stopping.
    Panic,
    /// Guest stopped without a declared reason (including a triple fault).
    Fault,

    /// Legacy guest/device exit-code path, or host-requested shutdown.
    /// This value alone is not proof of declared guest power-off.
    Shutdown { exit_code: i32 },

    /// Guest requested a reboot (KVM_SYSTEM_EVENT_RESET).
    RebootRequested,

    /// vCPU encountered a fatal error (triple fault, internal KVM error).
    Error { message: String },
}

/// Shared exit state: the VMM event loop stores `Some(VmExit)` here when the
/// VM exits, and `Context::run()` reads it to break the event loop.
pub type SharedVmExit = Arc<Mutex<Option<VmExit>>>;
