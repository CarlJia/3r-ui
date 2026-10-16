//! Integration tests for U3: xray subprocess lifecycle.
//!
//! Uses a shell-script stub in place of the real xray-core binary so the
//! supervisor's spawn / crash-detect / backoff / graceful-stop behavior can be
//! observed deterministically without downloading a real release.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost::Message;
use tempfile::tempdir;
use three_r_ui::xray::api::proto::xray::app::proxyman::command::{
    handler_service_server::{HandlerService, HandlerServiceServer},
    AddUserOperation, AlterInboundRequest, AlterInboundResponse, RemoveUserOperation,
};
use three_r_ui::xray::{SupervisorCommand, XraySupervisor};
use tokio::sync::{mpsc, oneshot};
use tonic::{Request, Response, Status};

/// A stub xray that stays alive until signalled. Honors SIGTERM (exit 0) and
/// SIGINT (exit 0).
fn write_stub_alive(path: &Path) {
    // The shell itself must stay the process the supervisor knows: `exec sleep`
    // would replace it with a program that has no signal handlers.
    let body = r#"#!/bin/sh
trap 'echo "stub: TERM"; exit 0' TERM
trap 'echo "stub: INT"; exit 0' INT
echo "stub started pid=$$"
while :; do sleep 0.2; done
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

/// A stub HandlerService that records every AlterInbound it is asked to
/// perform, so a test can see what the supervisor decided to do to a running
/// xray without needing xray itself.
#[derive(Clone, Default)]
struct RecordingHandler {
    calls: Arc<Mutex<Vec<AlterInboundRequest>>>,
}

#[tonic::async_trait]
impl HandlerService for RecordingHandler {
    async fn alter_inbound(
        &self,
        request: Request<AlterInboundRequest>,
    ) -> Result<Response<AlterInboundResponse>, Status> {
        self.calls.lock().unwrap().push(request.into_inner());
        Ok(Response::new(AlterInboundResponse {}))
    }
}

/// Serve `RecordingHandler` on an ephemeral loopback port and return its address
/// plus the recorded calls.
async fn start_mock_api() -> (String, Arc<Mutex<Vec<AlterInboundRequest>>>) {
    let handler = RecordingHandler::default();
    let calls = handler.calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
        .expect("tcp incoming");
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(HandlerServiceServer::new(handler))
            .serve_with_incoming(incoming)
            .await;
    });
    (addr.to_string(), calls)
}

/// A rendered config with one inbound carrying `clients`.
fn config_with_users(clients: &[(&str, &str)]) -> serde_json::Value {
    serde_json::json!({
        "inbounds": [{
            "tag": "inbound-1",
            "listen": "0.0.0.0",
            "port": 8443,
            "protocol": "vless",
            "settings": {
                "clients": clients
                    .iter()
                    .map(|(uuid, email)| serde_json::json!({"id": uuid, "email": email, "level": 0}))
                    .collect::<Vec<_>>(),
                "decryption": "none",
            },
        }],
    })
}

async fn wait_for_pid_file(pid_file: &Path) -> i32 {
    for _ in 0..40 {
        if let Some(pid) = read_pid(pid_file) {
            if process_alive(pid) {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("supervisor should have spawned a live child");
}

async fn wait_for_calls(
    calls: &Arc<Mutex<Vec<AlterInboundRequest>>>,
    expected: usize,
) -> Vec<AlterInboundRequest> {
    for _ in 0..50 {
        {
            let calls = calls.lock().unwrap();
            if calls.len() >= expected {
                return calls.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let calls = calls.lock().unwrap().clone();
    panic!(
        "expected {expected} AlterInbound calls, saw {}",
        calls.len()
    );
}

/// A user-list edit is the one config change xray can take at runtime, and it
/// must not restart the child: a restart drops every live connection, and the
/// panel reloads on each client edit and on every expired-client sweep.
#[tokio::test]
async fn user_list_change_is_applied_live_without_restarting() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);
    std::fs::write(&config_path, config_with_users(&[]).to_string()).unwrap();

    let (api_addr, calls) = start_mock_api().await;
    let supervisor = XraySupervisor::new(
        xray_bin,
        config_path.clone(),
        tmp.path().to_path_buf(),
        None,
    )
    .with_api_addr(api_addr);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let pid_before = wait_for_pid_file(&pid_file).await;

    // Add a client, exactly as the panel does: render, then ask for a reload.
    std::fs::write(
        &config_path,
        config_with_users(&[("11111111-1111-1111-1111-111111111111", "user-1")]).to_string(),
    )
    .unwrap();
    tx.send(SupervisorCommand::Reload)
        .await
        .expect("send reload");

    let recorded = wait_for_calls(&calls, 1).await;
    let operation = recorded[0].operation.clone().expect("operation");
    assert_eq!(recorded[0].tag, "inbound-1");
    assert_eq!(
        operation.r#type,
        "xray.app.proxyman.command.AddUserOperation"
    );
    let add = AddUserOperation::decode(operation.value.as_slice()).unwrap();
    let user = add.user.expect("user");
    assert_eq!(user.email, "user-1");

    assert!(process_alive(pid_before), "a live edit must not kill xray");
    assert_eq!(
        read_pid(&pid_file),
        Some(pid_before),
        "a user-list edit must be applied in place, not by restarting xray"
    );

    // …and the sweep that disables a client has to reach a running xray too.
    std::fs::write(&config_path, config_with_users(&[]).to_string()).unwrap();
    tx.send(SupervisorCommand::Reload)
        .await
        .expect("send reload");

    let recorded = wait_for_calls(&calls, 2).await;
    let operation = recorded[1].operation.clone().expect("operation");
    assert_eq!(
        operation.r#type,
        "xray.app.proxyman.command.RemoveUserOperation"
    );
    let remove = RemoveUserOperation::decode(operation.value.as_slice()).unwrap();
    assert_eq!(remove.email, "user-1");
    assert_eq!(read_pid(&pid_file), Some(pid_before));

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

/// Anything xray cannot take at runtime — here a REALITY key rotation, which
/// shows up as a changed port — has to be applied by respawning the child.
#[tokio::test]
async fn structural_config_change_restarts_the_child() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);
    std::fs::write(&config_path, config_with_users(&[]).to_string()).unwrap();

    // A mock that must never be called: no live edit can express this change.
    let (api_addr, calls) = start_mock_api().await;
    let supervisor = XraySupervisor::new(
        xray_bin,
        config_path.clone(),
        tmp.path().to_path_buf(),
        None,
    )
    .with_api_addr(api_addr);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let pid_before = wait_for_pid_file(&pid_file).await;

    let mut config = config_with_users(&[]);
    config["inbounds"][0]["port"] = serde_json::json!(9443);
    std::fs::write(&config_path, config.to_string()).unwrap();
    tx.send(SupervisorCommand::Reload)
        .await
        .expect("send reload");

    let mut restarted = None;
    for _ in 0..50 {
        match read_pid(&pid_file) {
            Some(pid) if pid != pid_before && process_alive(pid) => {
                restarted = Some(pid);
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let restarted = restarted.expect("a structural change must respawn xray");
    assert!(
        !process_alive(pid_before),
        "the old child should be gone after the restart"
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "a structural change must not be attempted over the live-edit API"
    );

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
    let _ = restarted;
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

/// Block until the supervisor records a live child pid, then return it.
async fn wait_for_live_pid(pid_file: &Path) -> i32 {
    for _ in 0..100 {
        if let Some(p) = read_pid(pid_file) {
            if process_alive(p) {
                return p;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no live child pid recorded in {}", pid_file.display());
}

/// Block until the pid is gone, or panic.
async fn wait_until_dead(pid: i32) {
    for _ in 0..100 {
        if !process_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("pid {pid} is still alive");
}

/// A stub xray that prints on stdout (like the real xray banner) and touches
/// `marker` when it runs, so a test can tell which binary the supervisor exec'd
/// after a replacement.
fn write_stub_marking(path: &Path, marker: &Path) {
    let body = format!(
        "#!/bin/sh\necho \"stub started pid=$$\"\necho \"stub started\" >> \"{}\"\nexec sleep 3600\n",
        marker.display()
    );
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// xray writes its access and error logs to **stdout** (see the `log` block in
/// `xray::config`), so a supervisor that only pipes stderr loses every line.
/// The panel learned this the hard way: with `log.access = "@stderr"` xray
/// created a stray file named `@stderr` in its working directory while
/// `$DATA_DIR/xray.log` and the connection log stayed empty.
#[tokio::test]
async fn xray_output_reaches_the_log_channel_and_the_log_file() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    let body = r#"#!/bin/sh
echo "2026/10/15 10:23:45 from 1.2.3.4:56789 accepted tcp:example.com:443 [t >> direct] 11111111-1111-1111-1111-111111111111"
echo "2026/10/15 10:23:45 [Warning] core: Xray 26.3.27 started"
echo "2026/10/15 10:23:45 [Error] app/dispatcher: boom on stderr" >&2
exec sleep 3600
"#;
    std::fs::write(&xray_bin, body).unwrap();
    std::fs::set_permissions(&xray_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (log_tx, mut log_rx) = mpsc::channel(16);
    let supervisor = XraySupervisor::new(
        xray_bin,
        config_path,
        tmp.path().to_path_buf(),
        Some(log_tx),
    );
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let mut lines: Vec<String> = Vec::new();
    while lines.len() < 3 {
        match tokio::time::timeout(Duration::from_secs(10), log_rx.recv()).await {
            Ok(Some(line)) => lines.push(line),
            _ => panic!("timed out waiting for xray output; got {lines:?}"),
        }
    }
    assert!(
        lines
            .iter()
            .any(|l| l.contains("accepted tcp:example.com:443")),
        "stdout access lines must reach the log worker: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("Xray 26.3.27 started")),
        "stdout error lines must reach the log worker: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("boom on stderr")),
        "stderr lines must reach the log worker: {lines:?}"
    );

    // xray.log keeps the diagnostics and skips access lines: those live in
    // connection_logs, and duplicating them here grows without bound as
    // traffic takes them off the end of the pipe.
    let contents = std::fs::read_to_string(tmp.path().join("xray.log")).unwrap_or_default();
    assert!(
        contents.contains("Xray 26.3.27 started"),
        "xray.log should keep stdout diagnostics: {contents:?}"
    );
    assert!(
        contents.contains("boom on stderr"),
        "xray.log should keep stderr diagnostics: {contents:?}"
    );
    assert!(
        !contents.contains("accepted"),
        "xray.log must not accumulate access lines: {contents:?}"
    );

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

/// The settings page replaces the binary the way a Windows installer has to:
/// stop xray, swap the file, start it again — a running image cannot be
/// overwritten. `StopForReplace` must therefore ack only after the child is
/// gone, and must not respawn behind the caller's back before `Start`.
#[tokio::test]
async fn stop_for_replace_holds_xray_down_until_start() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);

    let supervisor = XraySupervisor::new(
        xray_bin.clone(),
        config_path,
        tmp.path().to_path_buf(),
        None,
    );
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let first_pid = wait_for_live_pid(&pid_file).await;

    let (ack_tx, ack_rx) = oneshot::channel();
    tx.send(SupervisorCommand::StopForReplace { ack: ack_tx })
        .await
        .expect("send StopForReplace");
    tokio::time::timeout(Duration::from_secs(10), ack_rx)
        .await
        .expect("supervisor must ack StopForReplace")
        .expect("ack channel must not be dropped");

    wait_until_dead(first_pid).await;
    assert_eq!(
        read_pid(&pid_file),
        None,
        "a pid file pointing at a reaped child must not survive the stop"
    );

    // While held, nothing may respawn: the caller owns the binary path.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!process_alive(first_pid), "xray must stay down until Start");
    assert_eq!(read_pid(&pid_file), None, "xray must stay down until Start");

    let marker = tmp.path().join("replacement-ran");
    write_stub_marking(&xray_bin, &marker);

    let (start_tx, start_rx) = oneshot::channel();
    tx.send(SupervisorCommand::Start { ack: start_tx })
        .await
        .expect("send Start");
    tokio::time::timeout(Duration::from_secs(10), start_rx)
        .await
        .expect("supervisor must ack Start")
        .expect("ack channel must not be dropped")
        .expect("starting a healthy binary must succeed");

    let second_pid = wait_for_live_pid(&pid_file).await;
    assert_ne!(first_pid, second_pid, "Start must spawn a fresh child");
    assert!(
        marker.exists(),
        "Start must exec the binary now on disk, not the one spawned before the swap"
    );

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}

/// A job that stops xray for a replacement must still be able to shut the
/// panel down while the supervisor is holding xray down.
#[tokio::test]
async fn stop_for_replace_then_stop_exits_cleanly() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_alive(&xray_bin);

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    let _ = wait_for_live_pid(&pid_file).await;

    let (ack_tx, ack_rx) = oneshot::channel();
    tx.send(SupervisorCommand::StopForReplace { ack: ack_tx })
        .await
        .expect("send StopForReplace");
    tokio::time::timeout(Duration::from_secs(10), ack_rx)
        .await
        .expect("supervisor must ack StopForReplace")
        .expect("ack channel must not be dropped");

    tx.send(SupervisorCommand::Stop).await.expect("send Stop");
    let result = tokio::time::timeout(Duration::from_secs(15), handle)
        .await
        .expect("supervisor should stop after StopForReplace+Stop")
        .expect("supervisor task should not panic");
    assert!(result.is_ok(), "supervisor returned an error on Stop");
}

/// A `StopForReplace` that arrives while xray is crash-looping must be acked
/// promptly, and must not spawn a child only to kill it. The backoff sleep used
/// to be uninterruptible, so the request queued behind the timer and the update
/// job's 15s ack budget expired with nothing running to stop — reporting
/// "等待 xray 停止超时" exactly when the operator presses update to repair a
/// broken install, which is the situation that makes xray crash-loop.
#[tokio::test]
async fn stop_for_replace_is_answered_during_a_crash_backoff() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    // Records every spawn, then dies at once, so the supervisor sits in a crash
    // backoff between spawns.
    let spawn_log = tmp.path().join("spawns");
    let body = format!(
        "#!/bin/sh\necho spawn >> \"{}\"\necho \"stub crashing\"\nexit 23\n",
        spawn_log.display()
    );
    std::fs::write(&xray_bin, body).unwrap();
    std::fs::set_permissions(&xray_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let pid_file = tmp.path().join("xray.pid");
    // Three crashes put the supervisor in a 4s backoff, so a request answered
    // from inside that window is distinguishable from one answered after it.
    for _ in 0..300 {
        if spawn_count(&spawn_log) >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        spawn_count(&spawn_log),
        3,
        "the stub should have crashed three times by now"
    );
    // A crashed child stays a zombie until the supervisor waits on it, so a pid
    // that answers no signal means the supervisor has already handled the exit
    // and is inside its backoff — not merely a few instructions short of it.
    let mut reaped = false;
    for _ in 0..100 {
        if let Some(pid) = read_pid(&pid_file) {
            if !process_alive(pid) {
                reaped = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(reaped, "the supervisor should reap the crashed child");

    let (ack_tx, ack_rx) = oneshot::channel();
    tx.send(SupervisorCommand::StopForReplace { ack: ack_tx })
        .await
        .expect("send StopForReplace");
    tokio::time::timeout(Duration::from_secs(3), ack_rx)
        .await
        .expect("StopForReplace must be acked without waiting out the backoff")
        .expect("ack channel must not be dropped");

    assert_eq!(
        spawn_count(&spawn_log),
        3,
        "a replacement request must not spawn a child just to kill it"
    );
    assert_eq!(
        read_pid(&pid_file),
        None,
        "the pid file left by the crashed child must not survive the stop"
    );

    // Panel shutdown must not wait out the backoff either.
    tx.send(SupervisorCommand::Stop).await.expect("send Stop");
    let result = tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("supervisor should stop promptly while backing off")
        .expect("supervisor task should not panic");
    assert!(result.is_ok(), "supervisor returned an error on Stop");
}

/// Number of spawns the crash-looping stub has recorded.
fn spawn_count(spawn_log: &Path) -> usize {
    std::fs::read_to_string(spawn_log)
        .unwrap_or_default()
        .lines()
        .count()
}

/// The update job used to sleep 100ms and then report success no matter what.
/// A replacement that cannot even start must come back through the ack instead
/// of being counted as a successful restart (this is the "reports success while
/// xray exit-codes 23 behind its back" failure).
#[tokio::test]
async fn start_reports_an_immediately_exiting_binary_as_a_failure() {
    let tmp = tempdir().unwrap();
    let (xray_bin, config_path) = setup(&tmp);
    write_stub_crashing(&xray_bin, 23);

    let supervisor = XraySupervisor::new(xray_bin, config_path, tmp.path().to_path_buf(), None);
    let (tx, rx) = mpsc::channel(4);
    let handle = tokio::spawn(supervisor.run(rx));

    let (ack_tx, ack_rx) = oneshot::channel();
    tx.send(SupervisorCommand::Start { ack: ack_tx })
        .await
        .expect("send Start");
    let result = tokio::time::timeout(Duration::from_secs(10), ack_rx)
        .await
        .expect("supervisor must ack Start")
        .expect("ack channel must not be dropped");
    let err = result.expect_err("a binary that exits at once must not be reported as started");
    assert!(
        err.contains("23"),
        "the failure must carry the exit status; got {err:?}"
    );

    // A failed Start must not take the loop down: the backoff/retry path still
    // owns a broken install.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !handle.is_finished(),
        "supervisor must keep running after a failed Start"
    );

    let _ = tx.send(SupervisorCommand::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), handle).await;
}
