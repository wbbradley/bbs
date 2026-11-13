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
    // Hub's integration stream, its federation stream and the shared
    // messages they carry, vendored unchanged (see proto/hub/README.md). No
    // serde: the web UI never carries them. No transport constructor: it
    // would clash with the `Connect` RPCs, so a client is built from a channel
    // with `IntegrationClient::new`.
    let hub = protox::compile(
        [
            "bbs/integration/v1/integration.proto",
            "msgproto/v1/federation.proto",
        ],
        ["proto/hub"],
    )?;
    // The descriptors too, for the adapter's strict wire validation.
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR unset")?);
    std::fs::write(
        out.join("hub_descriptors.bin"),
        prost::Message::encode_to_vec(&hub),
    )?;
    // The room frames' large variants are boxed, so every frame and answer
    // is not as large as the largest.
    tonic_prost_build::configure()
        .build_transport(false)
        .boxed(".msgproto.v1.RoomAnswer.result.info")
        .boxed(".bbs.integration.v1.AdapterFrame.payload.remote_room_result")
        .boxed(".bbs.integration.v1.BbsFrame.payload.remote_room_request")
        .compile_fds(hub)?;
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
