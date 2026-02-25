// Copyright 2026, Red Hat Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration for vhost-user filesystem devices.

#[derive(Clone, Debug)]
pub struct VhostUserFsConfig {
    pub tag: String,
    pub socket_path: String,
    pub dax_window_mib: Option<u32>,
}
