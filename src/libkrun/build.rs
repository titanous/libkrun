fn main() {
    #[cfg(feature = "static-firmware")]
    {
        // Link libkrunfw statically. The static archive (libkrunfw.a) must be
        // on the library search path. In the Nix dev shell, LIBKRUNFW_LIB_PATH
        // points to the directory containing libkrunfw.a.
        if let Ok(path) = std::env::var("LIBKRUNFW_LIB_PATH") {
            println!("cargo:rustc-link-search=native={path}");
        }
        println!("cargo:rustc-link-lib=static=krunfw");
        println!("cargo:rerun-if-env-changed=LIBKRUNFW_LIB_PATH");
    }
}
