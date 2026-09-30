fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use the protoc binary vendored in protoc-bin-vendored so the build does
    // not depend on a system-wide protoc install.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    tonic_build::configure()
        .build_server(false)
        .compile_protos(&["protos/xray.proto"], &["protos/"])?;
    Ok(())
}
