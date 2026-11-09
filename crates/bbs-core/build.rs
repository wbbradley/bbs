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
    // Hub's integration stream and the shared messages it carries, vendored
    // unchanged (see proto/hub/README.md). No serde: the web UI never carries
    // them. No transport constructor: it would clash with the `Connect` RPC,
    // so a client is built from a channel with `IntegrationClient::new`.
    let hub = protox::compile(["bbs/integration/v1/integration.proto"], ["proto/hub"])?;
    tonic_prost_build::configure()
        .build_transport(false)
        .compile_fds(hub)?;
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
