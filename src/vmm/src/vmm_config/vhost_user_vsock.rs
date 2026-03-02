// Copyright 2026, Red Hat Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::net::UnixStream;

/// Connection method for vhost-user-vsock backend.
pub enum VhostUserVsockConnection {
    /// Connect via Unix domain socket path.
    SocketPath(String),
    /// Use a pre-connected UnixStream (fd-provisioned by orchestrator).
    Stream(UnixStream),
}

/// Configuration for a vhost-user-vsock device.
pub struct VhostUserVsockConfig {
    pub connection: VhostUserVsockConnection,
}

impl std::fmt::Debug for VhostUserVsockConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.connection {
            VhostUserVsockConnection::SocketPath(path) => {
                f.debug_struct("VhostUserVsockConfig")
                    .field("connection", &format!("SocketPath({})", path))
                    .finish()
            }
            VhostUserVsockConnection::Stream(_) => {
                f.debug_struct("VhostUserVsockConfig")
                    .field("connection", &"Stream(<fd>)")
                    .finish()
            }
        }
    }
}
