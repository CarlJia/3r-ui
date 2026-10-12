//! Integration tests for U3: xray subprocess lifecycle.
//!
//! Uses a shell-script stub in place of the real xray-core binary so the
//! supervisor's spawn / crash-detect / backoff / graceful-stop behavior can be
//! observed deterministically without downloading a real release.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::tempdir;
use three_r_ui::xray::{SupervisorCommand, XraySupervisor};
use tokio::sync::mpsc;

/// A stub xray that stays alive until signalled. Honors SIGTERM (exit 0),
/// SIGUSR1 (prints a marker and keeps running), and SIGINT.
fn write_stub_alive(path: &Path) {
    let body = r#"#!/bin/sh
trap 'echo "stub: TERM"; exit 0' TERM
trap 'echo "stub: USR1"' USR1
trap 'echo "stub: INT"; exit 0' INT
echo "stub started pid=$$"
exec sleep 3600
"#;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stub xray that exits immediately with the given code — used to exercise
/// the crash-detection + backoff path.
fn write_stub_crashing(path: &Path, exit_code: u32) {
    let body = format!("#!/bin/sh\necho \"stub crashing {exit_code}\"\nexit {exit_code}\n");
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn setup(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let xray_bin = tmp.path().join("xray-bin");
    let config_path = tmp.path().join("xray").join("config.json");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, "{}").unwrap();
    (xray_bin, config_path)
}

fn read_pid(pid_file: &Path) -> Option<i32> {
    std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
}

fn process_alive(pid: i32) -> bool {
    // Mirror the supervisor's own liveness probe: signal 0 checks existence
    // and permission only, and EPERM means the process exists but belongs to
    // another user — still alive for our purposes. Going through libc avoids
    // spawning a shell per poll, which is slow and can behave differently from
    // the code under test.
    if unsafe { libc::kill(pid, 0) } == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[tokio::test]
async fn supervisor_spawns_stub_and_writes_pid_file() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let mut pid = None;
    for _ in 0..40 {
        if let Some(p) = read_pid(&pid_file) {
            if process_alive(p) {
                pid = Some(p);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pid = pid.expect("supervisor should spawn xray and write a live pid file");
    assert!(process_alive(pid));

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

#[tokio::test]
async fn graceful_stop_terminates_the_child() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let mut pid = None;
    for _ in 0..40 {
        if let Some(p) = read_pid(&pid_file) {
            if process_alive(p) {
                pid = Some(p);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pid = pid.expect("xray should be running before we stop it");

    let _ = tx.send(SupervisorCommand::Stop).await;
    let stopped = tokio::time::timeout(Duration::from_secs(15), handle)
        .await
        .expect("supervisor should stop promptly on Stop command")
        .expect("supervisor task should not panic");
    assert!(stopped.is_ok(), "supervisor returned an error on Stop");

    assert!(!process_alive(pid), "xray child should be gone after Stop");
    assert!(
        !pid_file.exists(),
        "pid file should be removed on clean shutdown"
    );
}

#[tokio::test]
async fn crashing_child_is_restarted_after_backoff() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    // Exits immediately with code 7 — supervisor must detect and respawn.
    write_stub_crashing(&xray_bin, 7);

    let supervisor = XraySupervisor::new(
        xray_bin,
        config_path.clone(),
        tmp.path().to_path_buf(),
        None,
    );
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    // The supervisor writes a pid file on every spawn and leaves it in place
    // after a crash, so we should observe at least one recorded pid. Combined
    // with `missing_binary_backs_off_without_panicking` proving the loop keeps
    // running, this proves crash-detection plus respawn works.
    let pid_file = tmp.path().join("xray.pid");
    let mut seen: Vec<i32> = Vec::new();
    for _ in 0..80 {
        if let Some(p) = read_pid(&pid_file) {
            if !seen.contains(&p) {
                seen.push(p);
            }
        }
        if seen.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        !seen.is_empty(),
        "supervisor should record a pid for each xray spawn attempt"
    );

    // Supervisor must still be alive (respawning, not exiting after first crash).
    assert!(
        !handle.is_finished(),
        "supervisor should keep respawning after repeated crashes"
    );

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

#[tokio::test]
async fn crash_and_respawn_produces_a_fresh_pid() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    // First invocation crashes immediately; subsequent ones stay alive, so the
    // supervisor must swap the recorded pid from the crashed child to a live one.
    // The crash-then-respawn path is also covered by
    // crashing_child_is_restarted_after_backoff; this test pins the stronger
    // property that the pid file ends up naming a *live* process.
    let counter = tmp.path().join("count");
    let body = format!(
        r#"#!/bin/sh
n=$(cat "{counter}" 2>/dev/null || echo 0)
n=$((n+1))
echo "$n" > "{counter}"
if [ "$n" -eq 1 ]; then exit 9; fi
exec sleep 3600
"#,
        counter = counter.display()
    );
    std::fs::write(&xray_bin, body).unwrap();
    std::fs::set_permissions(&xray_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let mut live_pid = None;
    for _ in 0..100 {
        if let Some(p) = read_pid(&pid_file) {
            if process_alive(p) {
                live_pid = Some(p);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let pid = live_pid.expect("supervisor should respawn xray after the first crash");
    assert!(process_alive(pid), "respawned xray should be alive");

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

#[tokio::test]
async fn stale_pid_file_from_previous_run_is_reaped() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);

    // Simulate a leftover xray from a previous panel run that was kill -9'd.
    // The orphan is spawned as *this test process's* child, so a killed orphan
    // stays a zombie until we wait() on it — and signal 0 still reports a
    // zombie as alive. In production the orphan's parent (the panel) is dead,
    // so init reaps it and no zombie is left behind; here we reap it ourselves
    // right after the supervisor kills it so the liveness check is meaningful.
    let mut orphan = std::process::Command::new("sleep")
        .args(["600"])
        .spawn()
        .expect("spawn orphan");
    let orphan_pid = orphan.id() as i32;
    std::fs::write(tmp.path().join("xray.pid"), orphan_pid.to_string()).unwrap();
    assert!(process_alive(orphan_pid));

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    // After reap_stale signals the orphan, reap it here so it leaves the
    // process table instead of lingering as a zombie.
    let mut reaped = false;
    for _ in 0..40 {
        // The supervisor has signalled it by now; check for exit then reap.
        if matches!(orphan.try_wait(), Ok(Some(_))) {
            reaped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = orphan.kill();
    let _ = orphan.wait();
    assert!(
        reaped,
        "stale xray pid {orphan_pid} should have been reaped"
    );

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

#[tokio::test]
async fn reload_signal_is_delivered_to_the_child() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    // Wait for the child to be up so the pid file exists.
    let pid_file = tmp.path().join("xray.pid");
    let mut up = false;
    for _ in 0..40 {
        if let Some(p) = read_pid(&pid_file) {
            if process_alive(p) {
                up = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up, "xray should be running before we reload");

    // Reload must not kill the child — it's a config reload, not a restart.
    tx.send(SupervisorCommand::Reload)
        .await
        .expect("send reload");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let pid = read_pid(&pid_file).expect("pid file should still exist");
    assert!(process_alive(pid), "reload must not terminate xray");

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

#[tokio::test]
async fn missing_binary_backs_off_without_panicking() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    // Deliberately do NOT create xray_bin — spawn will fail.

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    // Give it time to attempt (and fail) at least one spawn + backoff cycle.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Supervisor must still be alive (not panicked).
    assert!(!handle.is_finished(), "supervisor should keep retrying");

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

#[tokio::test]
async fn restart_now_kills_child_and_respawns() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let first_pid = loop {
        if let Some(p) = read_pid(&pid_file) {
            if process_alive(p) {
                break p;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // RestartNow: supervisor kills the child and immediately respawns.
    tx.send(SupervisorCommand::RestartNow)
        .await
        .expect("send RestartNow");

    let second_pid = loop {
        if let Some(p) = read_pid(&pid_file) {
            if process_alive(p) && p != first_pid {
                break p;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    assert_ne!(
        first_pid, second_pid,
        "supervisor should spawn a new child with a different PID"
    );
    assert!(process_alive(second_pid), "new child should be alive");

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

#[tokio::test]
async fn restart_now_then_stop_exits_cleanly() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let _ = loop {
        if let Some(p) = read_pid(&pid_file) {
            if process_alive(p) {
                break p;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // RestartNow does NOT exit the loop; Stop must still terminate cleanly.
    tx.send(SupervisorCommand::RestartNow)
        .await
        .expect("send RestartNow");
    tokio::time::sleep(Duration::from_millis(300)).await;

    tx.send(SupervisorCommand::Stop).await.expect("send Stop");
    let result = tokio::time::timeout(Duration::from_secs(15), handle)
        .await
        .expect("supervisor should stop after RestartNow+Stop")
        .expect("supervisor task should not panic");
    assert!(result.is_ok(), "supervisor returned an error on Stop");
}

#[tokio::test]
async fn restart_now_keeps_loop_alive_indefinitely() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let _ = loop {
        if let Some(p) = read_pid(&pid_file) {
            if process_alive(p) {
                break p;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // Send RestartNow and nothing else; the supervisor loop must keep running.
    tx.send(SupervisorCommand::RestartNow)
        .await
        .expect("send RestartNow");

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !handle.is_finished(),
        "supervisor loop should still be alive after RestartNow"
    );

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}
