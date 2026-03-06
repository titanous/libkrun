fn main() {
    // When the embedded_init feature is enabled, copy the init binary into OUT_DIR
    // so that passthrough.rs can include it via include_bytes!(concat!(env!("OUT_DIR"), "/init.bin")).
    //
    // The init binary is searched in this order:
    //   1. KRUN_INIT_BIN env var (absolute path) -- set by `just mutants-integration` so that
    //      cargo-mutants (which copies the workspace without gitignored files) can still find it.
    //   2. ../../init/init relative to this package's directory -- the standard build path after
    //      `just build-init`.
    if std::env::var("CARGO_FEATURE_EMBEDDED_INIT").is_ok() {
        let init_path = std::env::var("KRUN_INIT_BIN").unwrap_or_else(|_| {
            // Default: relative to src/devices/ -- go up two levels to workspace root, then init/init.
            "../../init/init".to_string()
        });

        let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR not set by Cargo");
        let dest = std::path::Path::new(&out_dir).join("init.bin");

        std::fs::copy(&init_path, &dest).unwrap_or_else(|e| {
            panic!(
                "Failed to copy init binary from '{}' to '{}': {e}\n\
                 Hint: run `just build-init` first, or set KRUN_INIT_BIN to the absolute path.",
                init_path,
                dest.display()
            )
        });

        // Rebuild if the init binary or the env var changes.
        println!("cargo:rerun-if-changed={init_path}");
        println!("cargo:rerun-if-env-changed=KRUN_INIT_BIN");
    }
}
