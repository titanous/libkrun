//! Port of treefs c70edfb tests/guest.rs to the Nitro embedding backend.
use treefs::{
    reapi::{BlobStore, Digest, Directory, FileNode, MemoryBlobStore},
    reapi_server::Server,
    shared_workspace::SharedLayer,
    workspace::Layer,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        4,
        "usage: treefs-nitro-acceptance Image INITRAMFS none|dax"
    );
    let store = Box::leak(Box::new(MemoryBlobStore::default()));
    let blob = Digest::of(b"hello");
    store.put(blob, b"hello")?;
    let mut files = vec![FileNode {
        name: "hello".into(),
        digest: Some(blob.to_proto()),
        is_executable: false,
        node_properties: None,
    }];
    for i in 0..1000 {
        files.push(FileNode {
            name: format!("f{i:04}"),
            digest: Some(blob.to_proto()),
            is_executable: false,
            node_properties: None,
        });
    }
    let large: Vec<u8> = (0..64 * 1024 * 1024)
        .map(|offset| (offset % 251) as u8)
        .collect();
    let large_digest = Digest::of(&large);
    store.put(large_digest, &large)?;
    files.push(FileNode {
        name: "large".into(),
        digest: Some(large_digest.to_proto()),
        is_executable: false,
        node_properties: None,
    });
    files.sort_by(|left, right| left.name.cmp(&right.name));
    let root = Directory {
        files,
        ..Default::default()
    }
    .store(store)?;
    let empty = Directory::default().store(store)?;
    let state = tempfile::tempdir()?;
    let shared = SharedLayer::new(Layer::open(store, empty, state.path())?, store);
    let console = state.path().join("console.log");
    let mut builder = krun::Builder::new();
    builder.nitro_backend(krun::NitroBackendConfig {
        image: "unused".into(),
        cid: 215,
        watchdog: std::time::Duration::from_secs(10),
    });
    builder.disable_implicit_console()?;
    builder.vm_config(1, 1024)?;
    builder.external_kernel(
        args[1].clone().into(),
        krun::KernelFormat::Raw,
        Some(args[2].clone().into()),
    )?;
    builder.set_kernel_cmdline(vec!["console=hvc0", "quiet", "panic=-1", "rdinit=/init"]);
    builder.nitro_console_sink(
        Box::new(std::fs::File::create(&console)?),
        1024 * 1024,
        256 * 1024,
    );
    builder.add_virtiofs(
        "tree_ro",
        Box::new(treefs::adapter::Adapter(Server::new(
            store, root, b"digest",
        )?)),
        (args[3] == "dax").then_some(64 * 1024 * 1024),
    );
    builder.add_virtiofs(
        "tree_rw",
        Box::new(treefs::workspace_adapter::WorkspaceAdapter::from_shared(
            shared.clone(),
            b"digest",
        )),
        None,
    );
    let replacement_blob = Digest::of(b"new");
    store.put(replacement_blob, b"new")?;
    let replacement = Directory {
        files: vec![FileNode {
            name: "replaced".into(),
            digest: Some(replacement_blob.to_proto()),
            is_executable: false,
            node_properties: None,
        }],
        ..Default::default()
    }
    .store(store)?;
    let layer = shared.clone();
    let watched = console.clone();
    let watcher = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < 40 {
            if std::fs::read_to_string(&watched)
                .unwrap_or_default()
                .contains("TREEFS_REPLACE_READY")
            {
                let (changes, conflicts) = layer.replace(replacement).unwrap();
                assert!(conflicts.is_empty());
                assert!(changes.iter().any(|change| change.path() == b"replaced"));
                return;
            }
            std::thread::park_timeout(std::time::Duration::from_millis(20));
        }
        panic!("guest never reached replacement phase");
    });
    let exit = builder.build()?.run();
    let log = std::fs::read_to_string(&console)?;
    std::fs::write(format!("treefs-console-{}.log", args[3]), &log)?;
    if watcher.join().is_err() {
        panic!("replacement watcher failed: {exit:?}: {log}");
    }
    assert!(
        log.contains("TREEFS_ALL_OK") && !log.contains("TREEFS_FAIL"),
        "{exit:?}: {log}"
    );
    let captured = shared.capture()?;
    let server = Server::new(store, captured, b"digest")?;
    for i in 0..3000 {
        let path = format!("files/r{i:04}");
        if i % 3 == 0 {
            assert!(server.node_id(&path).is_err());
        } else {
            assert_eq!(
                server.read(server.node_id(&path)?, 0, 100)?,
                format!("data-{i:04}!").as_bytes()
            );
        }
    }
    assert_eq!(server.readlink(server.node_id("link")?)?, b"../ro/hello");
    println!("TREEFS_NITRO_OK exit={exit:?} files=3000 replacement=verified");
    Ok(())
}
