fn main() -> Result<(), Box<dyn std::error::Error>> {
    set_app_version();

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

fn set_app_version() {
    println!("cargo:rerun-if-env-changed=APP_VERSION");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    let version = detect_version();
    println!("cargo:rustc-env=APP_VERSION={}", version);
}

fn detect_version() -> String {
    // 1. 优先使用环境变量 APP_VERSION（允许构建环境或 CI 显式注入）
    if let Ok(v) = std::env::var("APP_VERSION") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return normalize_version(trimmed);
        }
    }

    // 2. 尝试从 git describe 获取最新 tag
    if let Ok(output) = std::process::Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
    {
        if output.status.success() {
            if let Ok(tag) = String::from_utf8(output.stdout) {
                let trimmed = tag.trim();
                if !trimmed.is_empty() {
                    return normalize_version(trimmed);
                }
            }
        }
    }

    // 3. Fallback 到 Cargo.toml 的 version
    normalize_version(env!("CARGO_PKG_VERSION"))
}

fn normalize_version(v: &str) -> String {
    if v.starts_with('v') || v.starts_with('V') {
        v.to_string()
    } else {
        format!("v{}", v)
    }
}
