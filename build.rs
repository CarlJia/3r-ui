fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use the protoc binary vendored in protoc-bin-vendored so the build does
    // not depend on a system-wide protoc install.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    // The client stubs are what the panel dials, but the tests stand up a mock
    // HandlerService to pin the live-edit path, so the server stubs are built
    // too.
    tonic_build::configure().compile_protos(
        &[
            // xray's own API surface, one file per vendored package.
            "protos/xray.proto",
            "protos/common/serial/typed_message.proto",
            "protos/common/protocol/user.proto",
            "protos/proxy/vless/account.proto",
            "protos/app/proxyman/command/handler.proto",
        ],
        &["protos/"],
    )?;
    Ok(())
}
