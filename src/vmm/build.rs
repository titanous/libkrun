fn main() {
    if std::env::var_os("CARGO_FEATURE_NITRO_SANDBOX").is_some()
        && (std::env::var_os("CARGO_FEATURE_TEE").is_some()
            || std::env::var_os("CARGO_FEATURE_AWS_NITRO").is_some())
    {
        eprintln!(
            "unsupported on this backend: nitro-sandbox cannot be combined with tee or aws-nitro"
        );
        std::process::exit(1);
    }
}
