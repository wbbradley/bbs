fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protox compiles the proto in pure Rust, so the build needs no `protoc`.
    let fds = protox::compile(["proto/bbs.proto"], ["proto"])?;
    let mut proto = tonic_prost_build::configure();
    if std::env::var_os("CARGO_FEATURE_SERDE").is_some() {
        // The web UI's JSON API carries these messages with their proto field names.
        proto = proto.message_attribute(
            ".",
            "#[derive(serde::Serialize, serde::Deserialize)] #[serde(default)]",
        );
    }
    proto.compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
