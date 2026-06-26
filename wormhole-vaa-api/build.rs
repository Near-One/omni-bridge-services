fn main() -> Result<(), Box<dyn std::error::Error>> {
    // prost-build 0.13 shells out to a `protoc` binary resolved from $PROTOC or
    // $PATH. Point it at the vendored protoc so the build is hermetic — no system
    // protobuf-compiler needed in CI, the Dockerfile, or for local `cargo build`.
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()?;
        // SAFETY: build scripts run single-threaded before any user code; this only
        // affects protoc resolution within this build process. `set_var` is `unsafe`
        // in edition 2024.
        unsafe {
            std::env::set_var("PROTOC", protoc);
        }
    }

    tonic_build::configure()
        .build_server(false)
        .compile_protos(&["proto/spy.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/spy.proto");
    Ok(())
}
