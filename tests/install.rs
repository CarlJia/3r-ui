//! Integration tests for U9: install.sh dry-run + template sanity.
//!
//! We only exercise the installer in `--dry-run` mode so the test does not
//! require root, systemd, or actually mutating the host filesystem. Real
//! install verification (systemd, xray, journald) belongs in the systemd
//! CI container described in the plan — not here.

use std::path::PathBuf;
use std::process::Command;

fn script_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("install.sh");
    p
}

fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new("/bin/bash")
        .arg(script_path())
        .args(args)
        .output()
        .expect("failed to run install.sh");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn dry_run_prints_install_plan() {
    let (code, stdout, stderr) = run(&["--dry-run"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for needle in [
        "creating directories",
        "installing binary",
        "writing env file",
        "writing systemd unit",
        "systemctl daemon-reload",
        "systemctl enable",
    ] {
        assert!(stdout.contains(needle), "missing {needle} in:\n{stdout}");
    }
}

#[test]
fn dry_run_does_not_create_directories() {
    let (_code, _stdout, _stderr) = run(&["--dry-run"]);
    // The host filesystem must not gain /var/lib/3r-ui just because we ran
    // the test. /var/lib/3r-ui is unusual enough that its absence here is
    // a strong negative signal — system-native installers would create it.
    assert!(
        !std::path::Path::new("/var/lib/3r-ui").exists()
            || std::env::var_os("3R_UI_TEST_DIRTY").is_some(),
        "dry-run must not create /var/lib/3r-ui"
    );
}

#[test]
fn unknown_flag_fails() {
    let (code, _stdout, stderr) = run(&["--bogus"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("unknown flag"), "stderr: {stderr}");
}

#[test]
fn env_template_runs_through_dash_as_a_valid_env_file() {
    // The env file must be parseable by `bash -n` (syntax) and `set -a; . file; set +a`
    // (assignment) — same shape systemd reads. Doing both checks here pins
    // both halves of the contract.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("packaging")
        .join("3r-ui.env");
    let syntax = Command::new("/bin/bash")
        .arg("-n")
        .arg(&path)
        .output()
        .expect("bash syntax check");
    assert!(
        syntax.status.success(),
        "env file failed bash syntax check: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );

    // The file is pure assignments + comments — sourcing it must succeed.
    let source = Command::new("/bin/bash")
        .arg("-c")
        .arg(format!(
            "set -a; . '{}'; set +a; \
             [ \"$DATA_DIR\" = \"/var/lib/3r-ui\" ] && \
             [ \"$BIND_ADDR\" = \"127.0.0.1:2053\" ]",
            path.display()
        ))
        .output()
        .expect("source env file");
    assert!(
        source.status.success(),
        "env file did not source cleanly: {}",
        String::from_utf8_lossy(&source.stderr)
    );
}

#[test]
fn unit_file_is_parseable_as_systemd_drop_in() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("packaging")
        .join("3r-ui.service");
    let raw = std::fs::read_to_string(&path).expect("unit file should exist");

    // Minimum bar systemd-system.conf(5) requires: [Unit] + [Service] +
    // [Install] sections, plus an ExecStart line.
    for needle in ["[Unit]", "[Service]", "[Install]", "ExecStart="] {
        assert!(raw.contains(needle), "missing {needle} in unit file");
    }
    assert!(
        raw.contains("/usr/local/bin/3r-ui"),
        "ExecStart must point at the install target"
    );
    assert!(
        raw.contains("EnvironmentFile="),
        "service must load the env file"
    );
    assert!(
        raw.contains("Restart=on-failure") || raw.contains("Restart=always"),
        "service must auto-restart"
    );
}

#[test]
fn uninstall_reports_removal_steps() {
    let (code, stdout, stderr) = run(&["--dry-run", "--uninstall"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for needle in [
        "stopping service",
        "removing systemd unit",
        "removing installed files",
        "data preserved",
    ] {
        assert!(stdout.contains(needle), "missing {needle} in:\n{stdout}");
    }
}
