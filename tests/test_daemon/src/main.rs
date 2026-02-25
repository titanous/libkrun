use std::sync::{Arc, RwLock};
use clap::Parser;
use vhost_user_backend::VhostUserDaemon;
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

mod backend;
mod filesystem;
mod fuse;

use backend::FsBackend;
use filesystem::SyntheticFs;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    socket_path: String,
    #[arg(long, default_value = "/dev/null")]
    shared_dir: String,  // Ignored, files are synthetic
}

fn main() -> Result<(), String> {
    env_logger::init();
    let args = Args::parse();

    // 1. Create backend with synthetic filesystem
    let fs = SyntheticFs::new();
    let backend = Arc::new(RwLock::new(FsBackend::new(fs)));

    log::info!("Created FsBackend with synthetic filesystem");
    log::info!("Socket path: {}", args.socket_path);
    log::info!("Shared dir: {}", args.shared_dir);

    // 2. Create vhost-user daemon
    // The crate handles:
    // - Listening on Unix socket
    // - Accepting connection from frontend
    // - Protocol message dispatch (GET_FEATURES, SET_MEM_TABLE, etc.)
    // - Vring setup and kick/call eventfd management
    // - Epoll-based event loop for vring kicks
    // - ADD_MEM_REGION for DAX window
    // - SET_DEVICE_STATE_FD / CHECK_DEVICE_STATE dispatch
    let mut daemon = VhostUserDaemon::new(
        "test-fs-daemon".to_string(),
        backend,
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    ).map_err(|e| format!("Failed to create daemon: {:?}", e))?;

    log::info!("VhostUserDaemon created successfully");

    // 3. Use serve() to listen on the socket and handle connections
    daemon.serve(&args.socket_path).map_err(|e| format!("Serve error: {}", e))?;

    log::info!("Daemon exiting");
    Ok(())
}
